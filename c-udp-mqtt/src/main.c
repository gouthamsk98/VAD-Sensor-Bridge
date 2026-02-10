/*
 * vad-sensor-bridge (C edition)
 *
 * High-performance multi-transport sensor data processor with VAD computation.
 *
 * Architecture:
 *   Input (one of): UDP / TCP / MQTT subscriber
 *     → shared lock-free MPMC ring buffer (raw sensor bytes)
 *       → N VAD processor threads (parse + compute VAD)
 *
 * Usage:
 *   ./vad-sensor-bridge --transport udp --port 9000
 *   ./vad-sensor-bridge --transport tcp --port 9000
 *   ./vad-sensor-bridge --transport mqtt --mqtt-host 127.0.0.1
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <sys/epoll.h>
#include <netinet/tcp.h>
#include <time.h>

#include <mosquitto.h>

#include "sensor.h"
#include "vad.h"
#include "stats.h"
#include "ring_buffer.h"

/* ─── Configuration defaults ─────────────────────────────────────── */
#define DEFAULT_PORT            9000
#define DEFAULT_MQTT_HOST       "127.0.0.1"
#define DEFAULT_MQTT_PORT       1883
#define DEFAULT_MQTT_TOPIC      "vad/sensors/+"
#define DEFAULT_CLIENT_ID       "vad-c-processor"
#define DEFAULT_RING_CAPACITY   262144
#define DEFAULT_RECV_BUF        (4 * 1024 * 1024)
#define DEFAULT_STATS_INTERVAL  5
#define DEFAULT_RECV_THREADS    4
#define DEFAULT_PROC_THREADS    2
#define MAX_RECV_THREADS        32
#define MAX_PROC_THREADS        16
#define MAX_DATAGRAM            65535
#define TCP_BACKLOG             128
#define MAX_TCP_CLIENTS         256

enum transport_mode {
    TRANSPORT_UDP  = 0,
    TRANSPORT_TCP  = 1,
    TRANSPORT_MQTT = 2,
};

static const char *transport_name[] = { "UDP", "TCP", "MQTT" };

/* ─── Global state ───────────────────────────────────────────────── */
static volatile sig_atomic_t g_running   = 1;
static stats_t               g_stats;

/* ─── Thread contexts ────────────────────────────────────────────── */
typedef struct {
    int                thread_id;
    int                port;
    int                recv_buf_size;
    ring_buffer_t     *ring;
} udp_thread_ctx_t;

typedef struct {
    int                port;
    int                recv_buf_size;
    ring_buffer_t     *ring;
} tcp_ctx_t;

typedef struct {
    int                thread_id;
    ring_buffer_t     *ring;
    int                stats_interval;
    int                is_stats_owner;
    const char        *transport_str;
} proc_thread_ctx_t;

/* ─── Signal handler ─────────────────────────────────────────────── */
static void sig_handler(int sig) {
    (void)sig;
    g_running = 0;
}

/* ─── UDP Receiver Thread ────────────────────────────────────────── */
static void *udp_receiver_thread(void *arg)
{
    udp_thread_ctx_t *ctx = (udp_thread_ctx_t *)arg;

    int fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (fd < 0) { perror("socket"); return NULL; }

    int opt = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &opt, sizeof(opt));
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));
    setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &ctx->recv_buf_size,
               sizeof(ctx->recv_buf_size));

    struct timeval tv = { .tv_sec = 1, .tv_usec = 0 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

    struct sockaddr_in addr = {
        .sin_family      = AF_INET,
        .sin_addr.s_addr = INADDR_ANY,
        .sin_port        = htons(ctx->port),
    };

    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind"); close(fd); return NULL;
    }

    printf("[UDP-%d] Listening on port %d\n", ctx->thread_id, ctx->port);

    uint8_t buf[MAX_DATAGRAM];

    while (g_running) {
        ssize_t n = recv(fd, buf, sizeof(buf), 0);
        if (n <= 0) {
            if (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)
                continue;
            break;
        }

        stats_record_recv(&g_stats, (uint64_t)n);

        if (ring_try_push(ctx->ring, buf, (uint16_t)n) != 0)
            stats_record_channel_drop(&g_stats);
    }

    close(fd);
    printf("[UDP-%d] Stopped\n", ctx->thread_id);
    return NULL;
}

/* ─── TCP Receiver Thread ────────────────────────────────────────── */
static void *tcp_receiver_thread(void *arg)
{
    tcp_ctx_t *ctx = (tcp_ctx_t *)arg;

    int listen_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (listen_fd < 0) { perror("socket"); return NULL; }

    int opt = 1;
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEPORT, &opt, sizeof(opt));

    struct sockaddr_in addr = {
        .sin_family      = AF_INET,
        .sin_addr.s_addr = INADDR_ANY,
        .sin_port        = htons(ctx->port),
    };

    if (bind(listen_fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind"); close(listen_fd); return NULL;
    }
    if (listen(listen_fd, TCP_BACKLOG) < 0) {
        perror("listen"); close(listen_fd); return NULL;
    }

    printf("[TCP] Listening on port %d\n", ctx->port);

    /* Set accept timeout so we can check g_running */
    struct timeval tv = { .tv_sec = 1, .tv_usec = 0 };
    setsockopt(listen_fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

    while (g_running) {
        struct sockaddr_in client_addr;
        socklen_t client_len = sizeof(client_addr);
        int client_fd = accept(listen_fd, (struct sockaddr *)&client_addr, &client_len);

        if (client_fd < 0) {
            if (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)
                continue;
            perror("accept");
            break;
        }

        /* Disable Nagle for low latency */
        int flag = 1;
        setsockopt(client_fd, IPPROTO_TCP, TCP_NODELAY, &flag, sizeof(flag));
        setsockopt(client_fd, SOL_SOCKET, SO_RCVBUF, &ctx->recv_buf_size,
                   sizeof(ctx->recv_buf_size));

        printf("[TCP] Client connected from %s:%d\n",
               inet_ntoa(client_addr.sin_addr), ntohs(client_addr.sin_port));

        /* Handle client inline (for benchmark, typically one client at a time).
         * Wire format: [ total_len: u32 LE ][ sensor_packet: total_len bytes ] */
        uint8_t len_buf[4];
        uint8_t pkt_buf[MAX_DATAGRAM];

        while (g_running) {
            /* Read 4-byte length prefix */
            ssize_t nr = 0;
            while (nr < 4) {
                ssize_t r = recv(client_fd, len_buf + nr, 4 - nr, 0);
                if (r <= 0) goto client_done;
                nr += r;
            }

            uint32_t msg_len = *(uint32_t *)len_buf;  /* LE on x86 */
            if (msg_len < SENSOR_HEADER_SIZE || msg_len > MAX_DATAGRAM) {
                stats_record_parse_error(&g_stats);
                continue;
            }

            /* Read the full packet */
            ssize_t total = 0;
            while ((size_t)total < msg_len) {
                ssize_t r = recv(client_fd, pkt_buf + total, msg_len - total, 0);
                if (r <= 0) goto client_done;
                total += r;
            }

            stats_record_recv(&g_stats, (uint64_t)(msg_len + 4));

            if (ring_try_push(ctx->ring, pkt_buf, (uint16_t)msg_len) != 0)
                stats_record_channel_drop(&g_stats);
        }

client_done:
        close(client_fd);
        printf("[TCP] Client disconnected\n");
    }

    close(listen_fd);
    printf("[TCP] Stopped\n");
    return NULL;
}

/* ─── MQTT Receiver (libmosquitto) ───────────────────────────────── */
/* Global ring for MQTT receiver to push into */
static ring_buffer_t *g_mqtt_ring = NULL;

static void mosq_on_message(struct mosquitto *mosq, void *obj,
                            const struct mosquitto_message *msg)
{
    (void)mosq; (void)obj;

    if (msg->payloadlen > 0) {
        stats_record_recv(&g_stats, (uint64_t)msg->payloadlen);

        if (ring_try_push(g_mqtt_ring, msg->payload,
                          (uint16_t)msg->payloadlen) != 0) {
            stats_record_channel_drop(&g_stats);
        }
    }
}

static void mosq_on_connect(struct mosquitto *mosq, void *obj, int rc)
{
    (void)obj;
    if (rc == 0) {
        printf("[MQTT] Connected, subscribing...\n");
        /* Subscribe on connect (and auto-reconnect) */
        mosquitto_subscribe(mosq, NULL, (const char *)mosquitto_userdata(mosq), 0);
    } else {
        fprintf(stderr, "[MQTT] Connect failed, rc=%d\n", rc);
    }
}

static void mosq_on_subscribe(struct mosquitto *mosq, void *obj, int mid,
                              int qos_count, const int *granted_qos)
{
    (void)mosq; (void)obj; (void)mid; (void)qos_count; (void)granted_qos;
    printf("[MQTT] Subscribed successfully\n");
}

/* ─── VAD Processor Thread ───────────────────────────────────────── */
static void *vad_processor_thread(void *arg)
{
    proc_thread_ctx_t *ctx = (proc_thread_ctx_t *)arg;

    printf("[VAD-%d] Processor thread started\n", ctx->thread_id);

    uint8_t raw[RING_SLOT_SIZE];
    uint16_t raw_len;
    sensor_packet_t pkt;

    struct timespec ts_last, ts_now;
    clock_gettime(CLOCK_MONOTONIC, &ts_last);

    while (g_running) {
        /* Periodic stats (only from thread 0) — always check, even when idle */
        if (ctx->is_stats_owner && ctx->stats_interval > 0) {
            clock_gettime(CLOCK_MONOTONIC, &ts_now);
            double elapsed = (ts_now.tv_sec - ts_last.tv_sec) +
                             (ts_now.tv_nsec - ts_last.tv_nsec) / 1e9;
            if (elapsed >= ctx->stats_interval) {
                stats_print_and_reset(&g_stats, elapsed, ctx->transport_str);
                ts_last = ts_now;
            }
        }

        if (ring_try_pop(ctx->ring, raw, &raw_len) != 0) {
            sched_yield();
            continue;
        }

        /* Parse sensor packet from raw bytes */
        if (sensor_parse_binary(raw, raw_len, &pkt) != 0) {
            stats_record_parse_error(&g_stats);
            continue;
        }

        /* Compute VAD (audio or emotional, routed by data_type) */
        vad_result_t result = vad_process(&pkt);
        stats_record_processed(&g_stats, result.is_active);
    }

    printf("[VAD-%d] Processor thread stopped\n", ctx->thread_id);
    return NULL;
}

/* ─── Main ───────────────────────────────────────────────────────── */
static void usage(const char *prog) {
    printf("Usage: %s [options]\n"
           "  --transport T      Transport: udp, tcp, mqtt (default: udp)\n"
           "  --port N           Listen port for UDP/TCP (default %d)\n"
           "  --mqtt-host H      MQTT broker host (default %s)\n"
           "  --mqtt-port N      MQTT broker port (default %d)\n"
           "  --mqtt-topic T     MQTT subscribe topic (default %s)\n"
           "  --recv-threads N   Receiver threads (default %d, UDP only)\n"
           "  --proc-threads N   VAD processor threads (default %d)\n"
           "  --ring-cap N       Ring buffer capacity (default %d)\n"
           "  --stats-interval N Stats interval secs (default %d, 0=off)\n"
           "  --help             Show this help\n",
           prog, DEFAULT_PORT, DEFAULT_MQTT_HOST, DEFAULT_MQTT_PORT,
           DEFAULT_MQTT_TOPIC, DEFAULT_RECV_THREADS, DEFAULT_PROC_THREADS,
           DEFAULT_RING_CAPACITY, DEFAULT_STATS_INTERVAL);
}

int main(int argc, char *argv[])
{
    int         port           = DEFAULT_PORT;
    const char *mqtt_host      = DEFAULT_MQTT_HOST;
    int         mqtt_port      = DEFAULT_MQTT_PORT;
    const char *mqtt_topic     = DEFAULT_MQTT_TOPIC;
    int         n_recv_threads = DEFAULT_RECV_THREADS;
    int         n_proc_threads = DEFAULT_PROC_THREADS;
    int         ring_cap       = DEFAULT_RING_CAPACITY;
    int         recv_buf       = DEFAULT_RECV_BUF;
    int         stats_interval = DEFAULT_STATS_INTERVAL;
    enum transport_mode transport = TRANSPORT_UDP;

    /* Arg parsing */
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--transport") && i+1 < argc) {
            i++;
            if (!strcmp(argv[i], "udp"))       transport = TRANSPORT_UDP;
            else if (!strcmp(argv[i], "tcp"))   transport = TRANSPORT_TCP;
            else if (!strcmp(argv[i], "mqtt"))  transport = TRANSPORT_MQTT;
            else { fprintf(stderr, "Unknown transport: %s\n", argv[i]); return 1; }
        }
        else if (!strcmp(argv[i], "--port") && i+1 < argc)
            port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--mqtt-host") && i+1 < argc)
            mqtt_host = argv[++i];
        else if (!strcmp(argv[i], "--mqtt-port") && i+1 < argc)
            mqtt_port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--mqtt-topic") && i+1 < argc)
            mqtt_topic = argv[++i];
        else if (!strcmp(argv[i], "--recv-threads") && i+1 < argc)
            n_recv_threads = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--proc-threads") && i+1 < argc)
            n_proc_threads = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--ring-cap") && i+1 < argc)
            ring_cap = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--stats-interval") && i+1 < argc)
            stats_interval = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--help")) {
            usage(argv[0]);
            return 0;
        }
    }

    if (n_recv_threads < 1) n_recv_threads = 1;
    if (n_recv_threads > MAX_RECV_THREADS) n_recv_threads = MAX_RECV_THREADS;
    if (n_proc_threads < 1) n_proc_threads = 1;
    if (n_proc_threads > MAX_PROC_THREADS) n_proc_threads = MAX_PROC_THREADS;

    printf("=== vad-sensor-bridge (C) ===\n");
    printf("Transport:       %s\n", transport_name[transport]);
    printf("Port:            %d\n", port);
    if (transport == TRANSPORT_MQTT)
        printf("MQTT broker:     %s:%d\n", mqtt_host, mqtt_port);
    printf("Recv threads:    %d\n", transport == TRANSPORT_UDP ? n_recv_threads : 1);
    printf("Proc threads:    %d\n", n_proc_threads);
    printf("Ring capacity:   %d\n", ring_cap);
    printf("Stats interval:  %ds\n", stats_interval);

    signal(SIGINT,  sig_handler);
    signal(SIGTERM, sig_handler);
    stats_init(&g_stats);

    /* Shared ring buffer: receivers → processors */
    ring_buffer_t ring;
    if (ring_init(&ring, ring_cap) != 0) {
        fprintf(stderr, "Failed to allocate ring buffer\n");
        return 1;
    }

    int n_cores = (int)sysconf(_SC_NPROCESSORS_ONLN);

    /* ── Spawn VAD processor threads ── */
    pthread_t proc_tids[MAX_PROC_THREADS];
    proc_thread_ctx_t proc_ctxs[MAX_PROC_THREADS];

    for (int i = 0; i < n_proc_threads; i++) {
        proc_ctxs[i].thread_id      = i;
        proc_ctxs[i].ring           = &ring;
        proc_ctxs[i].stats_interval = stats_interval;
        proc_ctxs[i].is_stats_owner = (i == 0);
        proc_ctxs[i].transport_str  = transport_name[transport];

        pthread_create(&proc_tids[i], NULL, vad_processor_thread, &proc_ctxs[i]);

        cpu_set_t cpuset;
        CPU_ZERO(&cpuset);
        CPU_SET(i % n_cores, &cpuset);
        pthread_setaffinity_np(proc_tids[i], sizeof(cpuset), &cpuset);
    }

    /* ── Start transport-specific receivers ── */
    pthread_t recv_tids[MAX_RECV_THREADS];
    int n_recv_started = 0;

    if (transport == TRANSPORT_UDP) {
        udp_thread_ctx_t udp_ctxs[MAX_RECV_THREADS];
        for (int i = 0; i < n_recv_threads; i++) {
            udp_ctxs[i].thread_id     = i;
            udp_ctxs[i].port          = port;
            udp_ctxs[i].recv_buf_size = recv_buf;
            udp_ctxs[i].ring          = &ring;

            pthread_create(&recv_tids[i], NULL, udp_receiver_thread, &udp_ctxs[i]);

            cpu_set_t cpuset;
            CPU_ZERO(&cpuset);
            CPU_SET((n_proc_threads + i) % n_cores, &cpuset);
            pthread_setaffinity_np(recv_tids[i], sizeof(cpuset), &cpuset);
            n_recv_started++;
        }

        printf("✅ All systems go — listening for sensor data via UDP\n");

        for (int i = 0; i < n_recv_started; i++)
            pthread_join(recv_tids[i], NULL);

    } else if (transport == TRANSPORT_TCP) {
        tcp_ctx_t tcp_ctx = {
            .port          = port,
            .recv_buf_size = recv_buf,
            .ring          = &ring,
        };

        pthread_create(&recv_tids[0], NULL, tcp_receiver_thread, &tcp_ctx);
        n_recv_started = 1;

        printf("✅ All systems go — listening for sensor data via TCP\n");

        pthread_join(recv_tids[0], NULL);

    } else if (transport == TRANSPORT_MQTT) {
        g_mqtt_ring = &ring;

        mosquitto_lib_init();

        struct mosquitto *mosq = mosquitto_new(DEFAULT_CLIENT_ID, true, (void *)mqtt_topic);
        if (!mosq) {
            fprintf(stderr, "MQTT: mosquitto_new failed\n");
            return 1;
        }

        mosquitto_message_callback_set(mosq, mosq_on_message);
        mosquitto_connect_callback_set(mosq, mosq_on_connect);
        mosquitto_subscribe_callback_set(mosq, mosq_on_subscribe);

        /* Increase internal receive buffer */
        mosquitto_int_option(mosq, MOSQ_OPT_RECEIVE_MAXIMUM, 65535);

        int rc = mosquitto_connect(mosq, mqtt_host, mqtt_port, 30);
        if (rc != MOSQ_ERR_SUCCESS) {
            fprintf(stderr, "MQTT connect failed: %s\n", mosquitto_strerror(rc));
            return 1;
        }

        printf("[MQTT] Connected to %s:%d\n", mqtt_host, mqtt_port);
        printf("✅ All systems go — listening for sensor data via MQTT\n");

        /* Start threaded network loop — processes messages on its own thread */
        rc = mosquitto_loop_start(mosq);
        if (rc != MOSQ_ERR_SUCCESS) {
            fprintf(stderr, "[MQTT] loop_start failed: %s\n", mosquitto_strerror(rc));
            return 1;
        }

        /* Wait for shutdown signal */
        while (g_running)
            sleep(1);

        mosquitto_loop_stop(mosq, true);
        mosquitto_disconnect(mosq);
        mosquitto_destroy(mosq);
        mosquitto_lib_cleanup();
    }

    /* Signal processors to stop and wait */
    g_running = 0;
    for (int i = 0; i < n_proc_threads; i++)
        pthread_join(proc_tids[i], NULL);

    ring_destroy(&ring);
    printf("Shutdown complete.\n");
    return 0;
}
