# 运行日志与延迟报告

本分支保存任务书要求的测量材料，源码与 main 同步。

- [A/B 正式汇总](ab/SUMMARY.md)：64 session、64B payload、delay 500µs，A/B 各 600 秒；包含全部要求的延迟指标及分位数。
- [A/C 对照汇总](ac/SUMMARY.md)：单 session、约 1000pps，各 60 秒，说明可比条件与收益。
- [A 的 60 秒演示日志](ab/a-60.log)及 [JSON](ab/a-60.json)
- [C 的逐包 ping 日志](ac/c.log.gz)
- [环境记录](environment.txt)
- [构建、单元测试、时钟、超时和脚本验证](verification/README.md)
- [原始 TSC 日志清单与 SHA-256](RAW-LOGS.tsv)及 [逐条校验记录](verification/trace-verification.log)

正式测量日期：2026-10-06（北京时间）。运行时测量基线为 59790e7c927ff21528a427c3d6b0c47132babb10；交付源码分支提交为 f28ff768ac95459ece9804847d42890a2ce4feb7。
运行时、DPDK、协议和指标记录的 Rust/C 源码与测量基线一致；后续只合并运行脚本、文档，并补全报告展示。

A/B 正式记录均正常完成 600 秒，TX/RX 相等，零超时、零丢包，mbuf 4095→4095。进程内耗时 A−B：p50 30ns、p99 60ns。
A/C 实际速率差异 0.114%；结论限于该单会话负载。

JSON 和终端日志完整保留。A/B 原始 TSC 打点体积较大，完整 .ticks.gz 保留在 EC2；绝对路径、条数、体积与 SHA-256 见清单。
读取原始打点使用 python3 scripts/report.py trace 文件.ticks.gz --limit 10。
