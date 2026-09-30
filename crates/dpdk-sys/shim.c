#include "shim.h"
#include <rte_eal.h>
#include <rte_errno.h>
#include <rte_ethdev.h>
#include <rte_mbuf.h>
#include <rte_timer.h>

static inline uint64_t cycles(void) {
    uint32_t lo, hi;
    __asm__ volatile("lfence; rdtsc; lfence" : "=a"(lo), "=d"(hi) :: "memory");
    return ((uint64_t)hi << 32) | lo;
}
int w_eal_init(int argc, char **argv) { return rte_eal_init(argc, argv); }
void w_eal_cleanup(void) { rte_eal_cleanup(); }
w_pool *w_pool_create(void) {
    return (w_pool *)rte_pktmbuf_pool_create("ping_pool", 4095, 0, 0, RTE_MBUF_DEFAULT_BUF_SIZE, rte_socket_id());
}
void w_pool_free(w_pool *p) { rte_mempool_free((struct rte_mempool *)p); }
unsigned w_pool_avail(w_pool *p) { return rte_mempool_avail_count((struct rte_mempool *)p); }
int w_port_start(w_pool *pool, uint8_t *mac) {
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
    *t2 = cycles(); // One timestamp for the whole burst, before any Rust wrappers.
    return n;
}
uint16_t w_tx(w_mbuf *m, uint64_t *t1) {
    struct rte_mbuf *packet = (struct rte_mbuf *)m;
    uint16_t n = rte_eth_tx_burst(0, 0, &packet, 1);
    *t1 = cycles();
    return n;
}
void w_maintenance(void) { rte_timer_manage(); }
void w_stats(uint64_t *v) {
    struct rte_eth_stats s = {0};
    rte_eth_stats_get(0, &s);
    v[0] = s.ipackets; v[1] = s.opackets; v[2] = s.imissed;
    v[3] = s.ierrors; v[4] = s.oerrors; v[5] = s.rx_nombuf;
}
const char *w_error(void) { return rte_strerror(rte_errno); }
