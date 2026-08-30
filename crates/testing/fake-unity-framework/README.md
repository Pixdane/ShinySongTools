# Fake UnityFramework

这是仅供无游戏测试使用的 `cdylib` fixture。它导出 `il2cpp-bridge-rs 0.1.4` 所需的完整符号集合，并为 runtime bootstrap fixture 提供可控的成功与失败路径；它不属于生产依赖图，也不会被打包进 `AKInterface.bundle`。

fixture 在实验仓库的 `il2cpp-bridge-rs-usage` fake runtime 基础上增加了第二个 `mscorlib` image、CRIWARE readiness 谓词和调用计数。

## 行为开关

测试进程可在调用前设置以下环境变量：

- `SCSP_FAKE_CACHE_FAIL`：让 `domain_get_assemblies` 返回空指针。
- `SCSP_FAKE_ATTACH_FAIL`：让 `il2cpp_thread_attach` 返回空指针。
- `SCSP_FAKE_TARGET_DRIFT`：让解析出的 method 名称变为 `Other`。
- `SCSP_FAKE_CRIWARE_NOT_READY`：让 readiness 谓词保持未完成。

## 测试自省导出

以下符号不属于 IL2CPP API，只供 fixture 断言调用模式：

- `scsp_fixture_domain_get_count`
- `scsp_fixture_detach_count`
- `scsp_fixture_criware_ready_count`
