//! confdeck 的核心逻辑：供应商数据、按 agent 生成配置、按键补丁写入各 agent 的配置文件。
//!
//! 这个 crate 不依赖任何 UI 框架。
//!
//! - [`provider`]：供应商、模型目录、每个 agent 的覆盖。
//! - [`preset`]：内置的常用供应商：地址、模型目录、给各 agent 的推荐值。
//! - [`store`]：confdeck 自己的数据文件 `~/.confdeck/providers.json`。
//! - [`agent`]：适配器，把供应商翻译成某个 agent 的配置补丁，只在内存里算。
//! - [`patch`]：按键补丁，只改点名的键，文件里其余内容原样保留。
//! - [`engine`]：写入引擎，改 agent 配置文件的唯一路径：备份、原子写入、写入期间被改了就重来。
//! - [`apply`]：把上面串起来，「把这家供应商写进这个 agent」；改了、删了供应商以后更新在用它的 agent。
//! - [`current`]：认出 agent 现在用的是哪家（单选连的哪家，多选勾了哪几家），只有 Key 不一样的算「Key 不一致」，
//!   对不上的算「自定义」，可以拼成一家新供应商；[`status`]：一次读全各 agent 现在的样子（界面重读用）。
//! - [`transfer`]：导入导出供应商（`providers.json` 的格式，可以不带 Key）。
//! - [`toggles`]：设一次就不动的开关（跳过首次确认、VS Code 插件跟着切换、署名），和供应商无关，按键补丁直接写。
//! - [`net`]：confdeck 自己发出去的请求（测连通、拉模型列表、下 models.dev），出站代理在这里生效；
//!   [`remote`]：对一家供应商测各个接口地址、拉模型列表；[`models_dev`]：按 models.dev 补模型的上下文、价格等。
//! - [`usage`]：扫各 agent 的会话记录算用量，认是哪家供应商、按价格算花费；[`balance`]：查供应商账户里还有多少钱；
//!   [`query`]：认不出的站点（new-api 一类的中转）按自己配的模板查余额；
//!   [`quota`]：官方订阅和套餐的限额窗口用了多少；[`session`]：各 agent 的会话列表、对话内容、恢复命令。
//! - [`account`]：官方账号多账号（Codex、Grok Build）：存下每个账号的登录，切换时写回 agent 的登录文件；
//!   [`program`]：找 agent 的命令行程序。
//! - [`backup`]、[`data`]、[`fsx`]：备份、数据目录、文件操作的基本件；[`diff`]：备份和现在的文件按行比。
//!
//! 按键补丁、写入引擎和各 agent 的关键字段清单移植自 ccs-lite（BlueOcean223/cc-switch，MIT 许可）
//! 的 `live/` 模块。

pub mod account;
pub mod agent;
pub mod apply;
pub mod backup;
pub mod balance;
pub mod current;
pub mod data;
pub mod diff;
pub mod engine;
pub mod fsx;
pub mod keychain;
pub mod models_dev;
pub mod net;
pub mod patch;
pub mod preset;
pub mod program;
pub mod provider;
pub mod query;
pub mod quota;
pub mod remote;
pub mod session;
pub mod status;
pub mod store;
pub mod toggles;
pub mod transfer;
pub mod usage;
