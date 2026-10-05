// 项目自己的 FFI 适配层：为 DPDK 头文件中的 inline API 导出可供 Rust 调用的符号。
// 这里调用已安装的 DPDK；并不修改 DPDK 驱动或 Linux 内核源码。
#define _GNU_SOURCE
#include "shim.h"
#include <rte_eal.h>
#include <rte_errno.h>
#include <rte_ethdev.h>
#include <rte_mbuf.h>
#include <rte_timer.h>
#include <rte_cycles.h>
#include <rte_lcore.h>
#include <rte_thread.h>
#include <stdatomic.h>
#include <x86intrin.h>

// 读取有序 TSC ticks；与 Rust now 使用相同屏障和指令，作为 C 层 T1/T2 打点。
static inline uint64_t cycles(void) {
    // 与 Rust metrics::now 相同的有序 TSC 读取，避免计时点被前后指令穿越。
    atomic_signal_fence(memory_order_seq_cst);
    _mm_lfence();
    uint64_t t = __rdtsc();
    _mm_lfence();
    atomic_signal_fence(memory_order_seq_cst);
    return t;
}
// 初始化进程级 EAL，argc/argv 指定核、网卡和内存模式；返回已处理参数数或负错误码。
int w_eal_init(int argc, char **argv) { return rte_eal_init(argc, argv); }
// 释放 EAL 全局资源；调用前须先关闭端口、释放所有 mbuf 和 mempool。
void w_eal_cleanup(void) { rte_eal_cleanup(); }
// 取得 EAL 初始化时确定的 TSC 每秒计数，供 Rust 进行 ticks 与时间单位换算。
uint64_t w_tsc_hz(void) { return rte_get_tsc_hz(); }
// 将调用线程绑定到指定 Linux CPU；返回 0 表示成功，非零表示绑核失败。
int w_pin_thread(unsigned core) {
    if (core >= CPU_SETSIZE) return -EINVAL;
    rte_cpuset_t cpus;
    CPU_ZERO(&cpus);
    CPU_SET(core, &cpus);
    return rte_thread_set_affinity(&cpus);
}
// 创建 4095 个报文缓冲的池，配置 128 个对象的本核缓存；失败时返回 NULL。
w_pool *w_pool_create(void) {
    // 4095 个 mbuf，单核缓存 128 个；常见分配/释放走本核缓存，减少共享池环访问。
    return (w_pool *)rte_pktmbuf_pool_create("ping_pool", 4095, 128, 0, RTE_MBUF_DEFAULT_BUF_SIZE, rte_socket_id());
}
// 销毁指定 mempool；调用者必须保证所有引用其缓冲的端口和 mbuf 已经释放。
void w_pool_free(w_pool *p) { rte_mempool_free((struct rte_mempool *)p); }
// 取得池内空闲 mbuf 总数，用于启动基线和关闭端口后的资源对账。
unsigned w_pool_avail(w_pool *p) { return rte_mempool_avail_count((struct rte_mempool *)p); }
// 配置端口 0 的单 RX/TX 队列并启动；pool 供 RX 使用，mac 输出本机 MAC，负值表示失败。
int w_port_start(w_pool *pool, uint8_t *mac) {
    // EAL 只允许一个 BDF，所以使用端口 0；A/B 都配置一条 RX 和一条 TX 队列。
    struct rte_eth_conf conf = {0};
    struct rte_eth_dev_info info;
    int rc = rte_eth_dev_info_get(0, &info);
    if (rc < 0) return rc;
    rc = rte_eth_dev_configure(0, 1, 1, &conf);
    if (rc < 0) return rc;
    uint16_t rx = 512, tx = 512;
    rc = rte_eth_dev_adjust_nb_rx_tx_desc(0, &rx, &tx);
    if (rc < 0) return rc;
    rc = rte_eth_rx_queue_setup(0, 0, rx, rte_socket_id(), &info.default_rxconf, (struct rte_mempool *)pool);
    if (rc < 0) return rc;
    // ENA 在空闲描述符低于阈值时清理 TX；将积攒量从约 64 降至约 16，缩短清理尖峰。
    // 这是 A/B 共用的队列参数，不是修改 ENA 驱动实现。
    info.default_txconf.tx_free_thresh = tx - 16;
    rc = rte_eth_tx_queue_setup(0, 0, tx, rte_socket_id(), &info.default_txconf);
    if (rc < 0) return rc;
    rc = rte_eth_macaddr_get(0, (struct rte_ether_addr *)mac);
    if (rc < 0) return rc;
    return rte_eth_dev_start(0);
}
// 停止并关闭端口 0，使驱动释放接收描述符和待回收发送缓冲。
void w_port_stop(void) { rte_eth_dev_stop(0); rte_eth_dev_close(0); }
// 从 pool 取一个 mbuf，并在已有数据区内预留 len 字节；失败归还已取对象并返回 NULL。
w_mbuf *w_alloc(w_pool *pool, uint16_t len) {
    struct rte_mbuf *m = rte_pktmbuf_alloc((struct rte_mempool *)pool);
    if (m && !rte_pktmbuf_append(m, len)) { rte_pktmbuf_free(m); return NULL; }
    return (w_mbuf *)m;
}
// 将调用者独占的 mbuf 归还原池；不能对已经交给 TX 驱动的对象重复调用。
void w_free(w_mbuf *m) { rte_pktmbuf_free((struct rte_mbuf *)m); }
// 返回 mbuf 当前有效数据起点，长度由 w_len 提供；指针只在 mbuf 存活期间有效。
uint8_t *w_data(w_mbuf *m) { return rte_pktmbuf_mtod((struct rte_mbuf *)m, uint8_t *); }
// 读取 mbuf 首段有效数据字节数；本项目的报文封装按单段帧使用。
uint16_t w_len(w_mbuf *m) { return ((struct rte_mbuf *)m)->data_len; }
// 最多把 count 个 RX mbuf 指针写入 out，返回实收个数；t2 输出整批共用的 burst 返回时刻。
uint16_t w_rx(w_mbuf **out, uint16_t count, uint64_t *t2) {
    uint16_t n = rte_eth_rx_burst(0, 0, (struct rte_mbuf **)out, count);
    *t2 = cycles(); // T2：本地 rx_burst 返回后，整批共用；不是网卡收到包时的硬件时间戳。
    return n;
}
// 尝试提交一个 mbuf，返回 1 才将所有权交给驱动；t1 输出 TX burst 返回时刻。
uint16_t w_tx(w_mbuf *m, uint64_t *t1) {
    struct rte_mbuf *packet = (struct rte_mbuf *)m;
    uint16_t n = rte_eth_tx_burst(0, 0, &packet, 1);
    *t1 = cycles(); // T1：本地 TX 提交返回；不代表对端已收到，也不等待物理发送完成。
    return n;
}
// 服务驱动注册的 DPDK timer（包括 ENA watchdog），由上层约每 1ms 调用。
// 运行 DPDK timer 管理函数，服务 ENA watchdog 等驱动维护任务。
void w_maintenance(void) { rte_timer_manage(); }
// 取得当前 rte_errno 对应的 DPDK 错误字符串；调用者只读取，不释放该指针。
const char *w_error(void) { return rte_strerror(rte_errno); }
