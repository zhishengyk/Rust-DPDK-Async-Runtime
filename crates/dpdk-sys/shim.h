#include <stdint.h>
typedef struct w_pool w_pool;
typedef struct w_mbuf w_mbuf;
int w_eal_init(int argc, char **argv);
void w_eal_cleanup(void);
w_pool *w_pool_create(void);
void w_pool_free(w_pool *pool);
unsigned w_pool_avail(w_pool *pool);
int w_port_start(w_pool *pool, uint8_t *mac);
void w_port_stop(void);
w_mbuf *w_alloc(w_pool *pool, uint16_t len);
void w_free(w_mbuf *m);
uint8_t *w_data(w_mbuf *m);
uint16_t w_len(w_mbuf *m);
uint16_t w_rx(w_mbuf **out, uint16_t count, uint64_t *t2);
uint16_t w_tx(w_mbuf *m, uint64_t *t1);
void w_maintenance(void);
void w_stats(uint64_t *values);
const char *w_error(void);
