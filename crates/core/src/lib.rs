//! confdeck 的核心逻辑：供应商数据、按 agent 生成配置、按键补丁写入各 agent 的配置文件。
//!
//! 这个 crate 不依赖任何 UI 框架。除了 [`store::save`]，其余都只在内存里算，不写盘。
//!
//! - [`provider`]：供应商、模型目录、每个 agent 的覆盖。
//! - [`store`]：confdeck 自己的数据文件 `~/.confdeck/providers.json`。
//! - [`agent`]：适配器，把供应商翻译成某个 agent 的配置补丁。
//! - [`patch`]：按键补丁，只改点名的键，文件里其余内容原样保留。
//!
//! 按键补丁和各 agent 的关键字段清单移植自 ccs-lite（BlueOcean223/cc-switch，MIT 许可）
//! 的 `live/` 模块。

pub mod agent;
pub mod patch;
pub mod provider;
pub mod store;
