//! confdeck 的核心逻辑：供应商数据、按 agent 生成配置、按键补丁写入各 agent 的配置文件。
//!
//! 这个 crate 不依赖任何 UI 框架。
//!
//! - [`provider`]：供应商、模型目录、每个 agent 的覆盖。
//! - [`store`]：confdeck 自己的数据文件 `~/.confdeck/providers.json`。
//! - [`agent`]：适配器，把供应商翻译成某个 agent 的配置补丁，只在内存里算。
//! - [`patch`]：按键补丁，只改点名的键，文件里其余内容原样保留。
//! - [`engine`]：写入引擎，改 agent 配置文件的唯一路径：备份、原子写入、写入期间被改了就重来。
//! - [`apply`]：把上面串起来，「把这家供应商写进这个 agent」；改了供应商以后写进所有在用它的 agent。
//! - [`current`]：认出 agent 现在用的是哪家（单选连的哪家，多选勾了哪几家），只有 Key 不一样的算「Key 不一致」，
//!   对不上的算「自定义」，可以拼成一家新供应商。
//! - [`backup`]、[`data`]、[`fsx`]：备份、数据目录、文件操作的基本件。
//!
//! 按键补丁、写入引擎和各 agent 的关键字段清单移植自 ccs-lite（BlueOcean223/cc-switch，MIT 许可）
//! 的 `live/` 模块。

pub mod agent;
pub mod apply;
pub mod backup;
pub mod current;
pub mod data;
pub mod engine;
pub mod fsx;
pub mod patch;
pub mod provider;
pub mod store;
