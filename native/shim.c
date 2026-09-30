// 项目自己的 FFI 适配层：为 DPDK 头文件中的 inline API 导出可供 Rust 调用的符号。
// 这里调用已安装的 DPDK；并不修改 DPDK 驱动或 Linux 内核源码。
#include "shim.h"
#include <rte_eal.h>
#include <rte_errno.h>
#include <rte_ethdev.h>
#include <rte_mbuf.h>
#include <rte_timer.h>

static inline uint64_t cycles(void) {
    // 与 Rust metrics::now 相同的有序 TSC 读取，避免计时点被前后指令穿越。
    uint32_t lo, hi;
    __asm__ volatile("lfence; rdtsc; lfence" : "=a"(lo), "=d"(hi) :: "memory");
    return ((uint64_t)hi << 32) | lo;
}
int w_eal_init(int argc, char **argv) { return rte_eal_init(argc, argv); }
void w_eal_cleanup(void) { rte_eal_cleanup(); }
w_pool *w_pool_create(void) {
    // 4095 个 mbuf，单核缓存 128 个；常见分配/释放走本核缓存，减少共享池环访问。
    return (w_pool *)rte_pktmbuf_pool_create("ping_pool", 4095, 128, 0, RTE_MBUF_DEFAULT_BUF_SIZE, rte_socket_id());
}
void w_pool_free(w_pool *p) { rte_mempool_free((struct rte_mempool *)p); }
unsigned w_pool_avail(w_pool *p) { return rte_mempool_avail_count((struct rte_mempool *)p); }
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
void w_port_stop(void) { rte_eth_dev_stop(0); rte_eth_dev_close(0); }
w_mbuf *w_alloc(w_pool *pool, uint16_t len) {
    struct rte_mbuf *m = rte_pktmbuf_alloc((struct rte_mempool *)pool);
    if (m && !rte_pktmbuf_append(m, len)) { rte_pktmbuf_free(m); return NULL; }
    return (w_mbuf *)m;
}
void w_free(w_mbuf *m) { rte_pktmbuf_free((struct rte_mbuf *)m); }
uint8_t *w_data(w_mbuf *m) { return rte_pktmbuf_mtod((struct rte_mbuf *)m, uint8_t *); }
uint16_t w_len(w_mbuf *m) { return ((struct rte_mbuf *)m)->data_len; }
uint16_t w_rx(w_mbuf **out, uint16_t count, uint64_t *t2) {
    uint16_t n = rte_eth_rx_burst(0, 0, (struct rte_mbuf **)out, count);
    *t2 = cycles(); // T2：本地 rx_burst 返回后，整批共用；不是网卡收到包时的硬件时间戳。
    return n;
}
uint16_t w_tx(w_mbuf *m, uint64_t *t1) {
    struct rte_mbuf *packet = (struct rte_mbuf *)m;
    uint16_t n = rte_eth_tx_burst(0, 0, &packet, 1);
    *t1 = cycles(); // T1：本地 TX 提交返回；不代表对端已收到，也不等待物理发送完成。
    return n;
}
// 服务驱动注册的 DPDK timer（包括 ENA watchdog），由上层约每 1ms 调用。
void w_maintenance(void) { rte_timer_manage(); }
void w_stats(uint64_t *v) {
    struct rte_eth_stats s = {0};
    rte_eth_stats_get(0, &s);
    v[0] = s.ipackets; v[1] = s.opackets; v[2] = s.imissed;
    v[3] = s.ierrors; v[4] = s.oerrors; v[5] = s.rx_nombuf;
}
const char *w_error(void) { return rte_strerror(rte_errno); }
