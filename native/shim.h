// 只暴露项目实际使用的 C ABI；Rust 不依赖 DPDK 内部结构布局。
#include <stdint.h>
// 不透明的 DPDK mempool 句柄；Rust 只通过指针使用，不依赖内部布局。
typedef struct w_pool w_pool;
// 不透明的 DPDK rte_mbuf 句柄；实际字段和数据缓冲由 DPDK 管理。
typedef struct w_mbuf w_mbuf;
// 初始化进程级 EAL，argc/argv 指定核、网卡和内存模式；返回已处理参数数或负错误码。
int w_eal_init(int argc, char **argv);
// 释放 EAL 全局资源；调用前须先关闭端口、释放所有 mbuf 和 mempool。
void w_eal_cleanup(void);
// 取得 EAL 初始化时确定的 TSC 每秒计数，供 Rust 进行 ticks 与时间单位换算。
uint64_t w_tsc_hz(void);
// Advertised per-packet RX hardware timestamp capability; not a DMA completion timestamp.
int w_rx_timestamp_supported(void);
// 将调用线程绑定到指定 Linux CPU；返回 0 表示成功，非零表示绑核失败。
int w_pin_thread(unsigned core);
// 创建 4095 个报文缓冲的池，配置 128 个对象的本核缓存；失败时返回 NULL。
w_pool *w_pool_create(void);
// 销毁指定 mempool；调用者必须保证所有引用其缓冲的端口和 mbuf 已经释放。
void w_pool_free(w_pool *pool);
// 取得池内空闲 mbuf 总数，用于启动基线和关闭端口后的资源对账。
unsigned w_pool_avail(w_pool *pool);
// 配置端口 0 的单 RX/TX 队列并启动；pool 供 RX 使用，mac 输出本机 MAC，负值表示失败。
int w_port_start(w_pool *pool, uint8_t *mac);
// 停止并关闭端口 0，使驱动释放接收描述符和待回收发送缓冲。
void w_port_stop(void);
// 从 pool 取一个 mbuf，并在已有数据区内预留 len 字节；失败归还已取对象并返回 NULL。
w_mbuf *w_alloc(w_pool *pool, uint16_t len);
// 将调用者独占的 mbuf 归还原池；不能对已经交给 TX 驱动的对象重复调用。
void w_free(w_mbuf *m);
// 返回 mbuf 当前有效数据起点，长度由 w_len 提供；指针只在 mbuf 存活期间有效。
uint8_t *w_data(w_mbuf *m);
// 读取 mbuf 首段有效数据字节数；本项目的报文封装按单段帧使用。
uint16_t w_len(w_mbuf *m);
// RX 返回的指针转交调用者持有；TX 仅在返回 1 时接管传入 mbuf。
// t1/t2 都由 shim 在对应 burst 返回后记录，供 A/B 使用一致的计时口径。
// 最多把 count 个 RX mbuf 指针写入 out，返回实收个数；t2 输出整批共用的 burst 返回时刻。
uint16_t w_rx(w_mbuf **out, uint16_t count, uint64_t *t2);
// 尝试提交一个 mbuf，返回 1 才将所有权交给驱动；t1 输出 TX burst 返回时刻。
uint16_t w_tx(w_mbuf *m, uint64_t *t1);
// 运行 DPDK timer 管理函数，服务 ENA watchdog 等驱动维护任务。
void w_maintenance(void);
// 向调用者的至少 6 项数组写入 RX、TX、missed、RX error、TX error、RX no-mbuf 计数。
void w_stats(uint64_t *values);
// 取得当前 rte_errno 对应的 DPDK 错误字符串；调用者只读取，不释放该指针。
const char *w_error(void);
