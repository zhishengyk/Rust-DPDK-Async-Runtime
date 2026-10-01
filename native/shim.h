// 只暴露项目实际使用的 C ABI；Rust 不依赖 DPDK 内部结构布局。
#include <stdint.h>
typedef struct w_pool w_pool;
typedef struct w_mbuf w_mbuf;
int w_eal_init(int argc, char **argv);
void w_eal_cleanup(void);
uint64_t w_tsc_hz(void);
int w_pin_thread(unsigned core);
w_pool *w_pool_create(void);
void w_pool_free(w_pool *pool);
unsigned w_pool_avail(w_pool *pool);
int w_port_start(w_pool *pool, uint8_t *mac);
void w_port_stop(void);
w_mbuf *w_alloc(w_pool *pool, uint16_t len);
void w_free(w_mbuf *m);
uint8_t *w_data(w_mbuf *m);
uint16_t w_len(w_mbuf *m);
// RX 返回的指针转交调用者持有；TX 仅在返回 1 时接管传入 mbuf。
// t1/t2 都由 shim 在对应 burst 返回后记录，供 A/B 使用一致的计时口径。
uint16_t w_rx(w_mbuf **out, uint16_t count, uint64_t *t2);
uint16_t w_tx(w_mbuf *m, uint64_t *t1);
void w_maintenance(void);
void w_stats(uint64_t *values);
const char *w_error(void);
