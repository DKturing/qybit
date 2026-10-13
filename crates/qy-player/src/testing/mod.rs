//! 测试设施（`cfg(test)` 或 `test-support` 特性）：夹具读取与独立参考、假平台 API
//! （整包与短写）、假优先级与假后端。解码与 pack 在测试中走真实实现，只替代平台调用。

mod fake;
mod fixtures;

pub use fake::*;
pub use fixtures::*;
