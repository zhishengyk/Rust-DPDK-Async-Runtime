# 运行日志和延迟报告

测量日期：2026-10-06（北京时间）。

- [A/B 汇总](ab/SUMMARY.md)：64 session、等待 500µs，各 600 秒。
- [A/C 汇总](ac/SUMMARY.md)：单 session、约 1000pps，各 60 秒。
- [环境及代码提交](environment.txt)
- [全量日志路径、条数与 SHA-256](RAW-LOGS.tsv)
- [逐条校验结果](verification/trace-verification.log)

JSON 和终端日志位于对应目录；C 的逐包日志为 c.log.gz。60 秒演示只保留记录，不列入汇总。
A/B 全量原始打点为 EC2 上的 .ticks.gz 文件，具体绝对路径见清单；大文件未放入 Git。gzip 为无损压缩，用 scripts/read-trace.py 可逐条导出 CSV。格式及 T0–T5 定义见仓库 README。

逐条检查了事件数、时间顺序和各指标最大值，并与 JSON 对账；日志没有采样。正常结束时刷盘，全量记录仍有复制和 I/O 开销。

本机 100 万次时钟读取的差值 gcd 为 26 ticks，DPDK TSC 频率为 2.6GHz，约 10ns 一步。正式日志的差值也已检查；ns 是单位，不代表 1ns 分辨率。原始 TSC 不取整，HDR 在低于 262144ns 范围为 1ns 桶。
