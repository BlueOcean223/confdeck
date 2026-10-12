//! 阶段 3 的几样（decisions「功能范围」）：出站代理、测连通、从供应商拉模型列表、按 models.dev 补参数、导入导出。
//! core 里都是阻塞调用，这里放到后台线程，结果回到 `Store` 再通知界面。
//!
//! - **测连通**的结果按地址记（[`NetState::reach`]），供应商列表和编辑器共用：编辑器里改了地址，新地址自然没有结果。
//! - **models.dev** 下过一次就留在进程里（磁盘上还有一天的缓存，见 core 的 `models_dev`）。
//! - 每次联网现建一个客户端（core 的 `Http::new`），代理设置改了下一次就生效。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use confdeck_core::apply::{self, Synced};
use confdeck_core::fsx::{self, Mode};
use confdeck_core::models_dev::{self, ModelsDev};
use confdeck_core::net::{Http, NetError, Proxy, Unreachable};
use confdeck_core::provider::{Model, Provider};
use confdeck_core::remote;
use confdeck_core::store::StoreError;
use confdeck_core::transfer::{self, Plan};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Notice, Store};
use crate::theme::Theme;
use crate::ui;

#[derive(Default)]
pub struct NetState {
    /// 地址 → 测连通的结果。
    reach: HashMap<String, Reach>,
    models_dev: Option<Arc<ModelsDev>>,
}

#[derive(Clone, Debug)]
pub enum Reach {
    Checking,
    Done(Result<Duration, NetError>),
}

impl Store {
    /// 设置页的出站代理。
    pub fn proxy(&self) -> Proxy {
        self.engine.proxy()
    }

    /// 改出站代理（存在 `state.json`）。存不了报错。
    pub fn set_proxy(&mut self, proxy: Proxy, cx: &mut Context<Self>) {
        if let Err(err) = self.engine.set_proxy(proxy) {
            self.announce(
                &[Notice::err(t!(
                    "net.proxy_not_saved",
                    error = err.to_string()
                ))],
                cx,
            );
        }
        cx.notify();
    }

    /// 按 `proxy` 连一次 models.dev（confdeck 自己要用的地址），看代理能不能用。
    pub fn test_proxy(
        &self,
        proxy: Proxy,
        cx: &mut Context<Self>,
    ) -> Task<Result<Duration, NetError>> {
        cx.background_executor()
            .spawn(async move { Http::new(&proxy)?.reach(models_dev::URL) })
    }

    /// 这个地址测过没有、结果是什么。
    pub fn reach(&self, url: &str) -> Option<&Reach> {
        self.net.reach.get(url)
    }

    /// 有没有正在测的。
    pub fn checking(&self) -> bool {
        self.net
            .reach
            .values()
            .any(|r| matches!(r, Reach::Checking))
    }

    /// 一家的结果合成一个（供应商列表那一行用）：有在测的算在测，有不通的算不通，都通了取最慢的。
    /// 另外给出每个地址各自的结果（地址相同的几种协议算一个，标签是「Anthropic / OpenAI Chat」），悬停时列出来。
    pub fn provider_reach(&self, provider: &Provider) -> Option<(Reach, Vec<(String, Reach)>)> {
        let mut each: Vec<(&str, String, Reach)> = Vec::new();
        for (protocol, url) in &provider.endpoints {
            let Some(reach) = self.reach(url) else {
                continue;
            };
            match each.iter_mut().find(|(seen, _, _)| *seen == url) {
                Some((_, label, _)) => *label = format!("{label} / {protocol}"),
                None => each.push((url, protocol.to_string(), reach.clone())),
            }
        }
        let checking = each.iter().any(|(_, _, r)| matches!(r, Reach::Checking));
        let failed = each.iter().find_map(|(_, _, r)| match r {
            Reach::Done(Err(err)) => Some(err.clone()),
            _ => None,
        });
        let slowest = each
            .iter()
            .filter_map(|(_, _, r)| match r {
                Reach::Done(Ok(duration)) => Some(*duration),
                _ => None,
            })
            .max();
        let all = if checking {
            Reach::Checking
        } else if let Some(err) = failed {
            Reach::Done(Err(err))
        } else {
            Reach::Done(Ok(slowest?))
        };
        Some((
            all,
            each.into_iter()
                .map(|(_, label, reach)| (label, reach))
                .collect(),
        ))
    }

    /// 测这几家的每个接口地址，一家一个后台任务（各家同时测）。
    pub fn check_reach(&mut self, providers: Vec<Provider>, cx: &mut Context<Self>) {
        let proxy = self.proxy();
        for provider in providers {
            for url in provider.endpoints.values() {
                self.net.reach.insert(url.clone(), Reach::Checking);
            }
            let proxy = proxy.clone();
            let task = cx.background_executor().spawn(async move {
                match Http::new(&proxy) {
                    Ok(http) => remote::check(&http, &provider)
                        .into_iter()
                        .map(|reach| (reach.url, reach.result))
                        .collect::<Vec<_>>(),
                    Err(err) => provider
                        .endpoints
                        .values()
                        .map(|url| (url.clone(), Err(err.clone())))
                        .collect(),
                }
            });
            cx.spawn(async move |this, cx| {
                let results = task.await;
                this.update(cx, |store, cx| {
                    for (url, result) in results {
                        store.net.reach.insert(url, Reach::Done(result));
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// 从这家的接口拉模型列表，按预设和 models.dev 补好参数（`provider` 可以是编辑器里还没存的样子）。
    pub fn fetch_models(
        &mut self,
        provider: Provider,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Model>, NetError>> {
        let proxy = self.proxy();
        let data = self.engine.data().clone();
        let cached = self.net.models_dev.clone();
        let task = cx.background_executor().spawn(async move {
            let http = Http::new(&proxy)?;
            let mut models = remote::list_models(&http, &provider)?;
            // models.dev 下不下来也不要紧，列表照样给。
            let dev = cached.or_else(|| models_dev::load(&data, &http).ok().map(Arc::new));
            for model in &mut models {
                if let Some(known) = models_dev::known(&provider, &model.id, dev.as_deref()) {
                    models_dev::fill(model, &known);
                }
            }
            Ok::<_, NetError>((models, dev))
        });
        cx.spawn(async move |this, cx| {
            let (models, dev) = task.await?;
            this.update(cx, |store, _| {
                store.net.models_dev = dev.or(store.net.models_dev.take())
            })
            .ok();
            Ok(models)
        })
    }

    /// 给这家目录里的模型补上空着的参数（编辑器的「补全」）。返回补过的目录和补了几个。
    pub fn fill_models(
        &mut self,
        provider: Provider,
        cx: &mut Context<Self>,
    ) -> Task<Result<(Vec<Model>, usize), NetError>> {
        let proxy = self.proxy();
        let data = self.engine.data().clone();
        let cached = self.net.models_dev.clone();
        let task = cx.background_executor().spawn(async move {
            let dev = match cached {
                Some(dev) => dev,
                None => Arc::new(models_dev::load(&data, &Http::new(&proxy)?)?),
            };
            let mut models = provider.models.clone();
            let mut filled = 0;
            for model in &mut models {
                if let Some(known) = models_dev::known(&provider, &model.id, Some(&dev))
                    && models_dev::fill(model, &known)
                {
                    filled += 1;
                }
            }
            Ok::<_, NetError>((models, filled, dev))
        });
        cx.spawn(async move |this, cx| {
            let (models, filled, dev) = task.await?;
            this.update(cx, |store, _| store.net.models_dev = Some(dev))
                .ok();
            Ok((models, filled))
        })
    }

    /// 导出供应商到 `path`。`keys` 为假时不带 Key。
    pub fn export(&self, path: &Path, keys: bool, cx: &mut Context<Self>) {
        let bytes = transfer::export(&self.providers, keys);
        let mode = if keys { Mode::Private } else { Mode::Keep };
        let notice = match fsx::write_atomic(path, &bytes, mode) {
            Ok(()) => Notice::ok(t!("transfer.exported", file = self.tilde(path))),
            Err(err) => Notice::err(t!("transfer.export_failed", error = err.to_string())),
        };
        self.announce(&[notice], cx);
    }

    /// 读一个要导入的文件，算出会改什么。
    pub fn read_import(&self, path: &Path) -> Result<(Vec<Provider>, Plan), String> {
        let bytes = std::fs::read(path).map_err(|err| err.to_string())?;
        let incoming = transfer::read(&bytes, path).map_err(|err| describe_store(&err))?;
        let plan = transfer::plan(&self.providers, &incoming);
        Ok((incoming, plan))
    }

    /// 导入：存一次列表，换掉的那几家写进在用它们的 agent（后台写，Codex 可能要重写模型目录）。
    pub fn import(&mut self, incoming: Vec<Provider>, cx: &mut Context<Self>) {
        let count = transfer::plan(&self.providers, &incoming).changes();
        let engine = self.engine.clone();
        let home = self.home.clone();
        let providers = self.providers.clone();
        let task = cx
            .background_executor()
            .spawn(async move { apply::import(&engine, &home, &providers, &incoming) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                store.reload();
                let notices = describe_import(count, result);
                store.announce(&notices, cx);
            })
            .ok();
        })
        .detach();
    }
}

fn describe_import(count: usize, result: Result<Vec<Synced>, StoreError>) -> Vec<Notice> {
    let synced = match result {
        Ok(synced) => synced,
        Err(err) => {
            return vec![Notice::err(t!(
                "transfer.import_failed",
                error = describe_store(&err)
            ))];
        }
    };
    let mut notices = vec![Notice::ok(if count == 1 {
        t!("transfer.imported_one")
    } else {
        t!("transfer.imported_other", count = count)
    })];
    for failed in synced.iter().filter(|s| s.result.is_err()) {
        if let Err(err) = &failed.result {
            notices.push(Notice::err(t!(
                "toast.sync_failed",
                agent = failed.agent.to_string(),
                error = err.to_string()
            )));
        }
    }
    notices
}

fn describe_store(err: &StoreError) -> String {
    match err {
        StoreError::Parse { line, .. } => t!("transfer.bad_file", line = line).into(),
        StoreError::Version { .. } => t!("transfer.too_new").into(),
        other => other.to_string(),
    }
}

/// 联网出错的一句话（连通结果、代理测试、拉列表都用它）。
pub fn describe(err: &NetError) -> SharedString {
    match err {
        NetError::BadProxy { .. } => t!("net.bad_proxy"),
        NetError::BadUrl { .. } => t!("net.bad_url"),
        NetError::Timeout => t!("net.timeout"),
        NetError::Unreachable(Unreachable::Dns) => t!("net.dns"),
        NetError::Unreachable(Unreachable::Refused) => t!("net.refused"),
        NetError::Unreachable(Unreachable::Tls) => t!("net.tls"),
        NetError::Unreachable(Unreachable::Proxy) => t!("net.proxy"),
        NetError::Unreachable(Unreachable::Closed) => t!("net.closed"),
        NetError::Unreachable(Unreachable::Other(detail)) => {
            t!("net.unreachable", detail = detail.clone())
        }
        NetError::Status {
            status: status @ (401 | 403),
            ..
        } => t!("net.key_rejected", status = status),
        NetError::Status { status, .. } => t!("net.status", status = status),
        NetError::Parse { .. } => t!("net.parse"),
    }
    .into()
}

/// 拉模型列表出错：404 说的是「这家没有列表接口」，别的同 [`describe`]。
pub fn describe_listing(err: &NetError) -> SharedString {
    match err {
        NetError::Status {
            status: 404 | 405, ..
        } => t!("net.no_model_list").into(),
        other => describe(other),
    }
}

/// 往返时间，`86 ms`、`1.2 s`。
pub fn latency(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

/// 测连通结果的一句话：「…」、往返时间、不通的原因。
pub fn reach_line(reach: &Reach) -> SharedString {
    match reach {
        Reach::Checking => "…".into(),
        Reach::Done(Ok(duration)) => latency(*duration).into(),
        Reach::Done(Err(err)) => describe(err),
    }
}

/// 测连通结果的小字：测着是「…」，通了是往返时间（绿），不通是原因（红，长了截断，悬停看全）。
pub fn reach_text(t: &Theme, id: impl Into<ElementId>, reach: &Reach) -> Stateful<Div> {
    let text = reach_line(reach);
    let color = match reach {
        Reach::Checking => t.muted,
        Reach::Done(Ok(_)) => t.success,
        Reach::Done(Err(_)) => t.danger,
    };
    let failed = matches!(reach, Reach::Done(Err(_)));
    div()
        .id(id)
        .flex_none()
        .max_w(px(180.))
        .truncate()
        .text_size(px(11.5))
        .text_color(color)
        .when(failed, |d| d.tooltip(ui::tooltip(text.clone())))
        .child(text)
}
