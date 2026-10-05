# 运行日志和延迟报告

测量日期：2026-10-06（北京时间）。

- [A/B 汇总](ab/SUMMARY.md)：64 session、等待 500µs，各 600 秒。
- [A/C 汇总](ac/SUMMARY.md)：单 session、约 1000pps，各 60 秒。
- [环境及代码提交](environment.txt)

原始日志和 JSON 位于对应目录；C 原始日志为 c.log.gz。60 秒演示只保留原始记录。
A/B 汇总使用时间戳公式，发送段、接收段分别统计；两段之和按逐包计算。
