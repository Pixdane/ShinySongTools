//! 编译失败：Update 形态的函数（首参 `UpdateCtx`）注册为 Startup system
//! 必须被类型系统拒绝（跨 phase 注册是编译错误）。

use scsp_plugin_api::{AppCtx, Plugin, PluginError, UpdateCtx};

fn update_system(_ctx: UpdateCtx<'_>) -> Result<(), PluginError> {
    Ok(())
}

struct BadPlugin;

impl Plugin for BadPlugin {
    fn name(&self) -> &'static str {
        "bad"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        ctx.add_startup_system(update_system);
        Ok(())
    }
}

fn main() {
    let _ = BadPlugin;
}
