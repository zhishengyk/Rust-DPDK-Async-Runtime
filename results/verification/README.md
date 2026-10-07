# 验证记录

正式测量对应代码提交 59790e7c927ff21528a427c3d6b0c47132babb10。
保留构建、单元测试、Clippy、时钟精度、全量打点校验、超时对账和打点开销对照。
sources-at-measurement.sha256 是测量当时的源码清单；之后仅整理了运行脚本和文档。
a-60-audit-20261007.json / .log 是 2026-10-07 复跑的 60 秒演示。
临时对照实验的全量 trace 已清除，相关摘要中的输出路径为当时的运行路径；正式 A/B/C 原始打点完整保留。
timeout.ticks.gz 可用 python3 scripts/report.py trace 查看。
