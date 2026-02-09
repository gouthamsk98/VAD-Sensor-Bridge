/*
 * vad-sensor-bridge (C edition)
 *
 * High-performance UDP → MQTT QoS 0 sensor data bridge.
 *
 * Architecture:
 *   N UDP receiver threads (SO_REUSEPORT)
 *     → per-thread lock-free ring buffer
 *       → 1 MQTT publisher thread (paho.mqtt.c)
 *
 * Build: see Makefile
 * Deps:  libpaho-mqtt3a (async client), pthreads
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
#include <arpa/inet.h>
#include <sys/socket.h>
#include <time.h>

#include "MQTTAsync.h"

#include "sensor.h"
#include "stats.h"
#include "ring_buffer.h"

/* ─── Configuration defaults ─────────────────────────────────────── */
#define DEFAULT_UDP_PORT       9000
#define DEFAULT_MQTT_HOST      "127.0.0.1"
#define DEFAULT_MQTT_PORT      1883
#define DEFAULT_TOPIC_PREFIX   "vad/sensors"
#define DEFAULT_CLIENT_ID      "vad-c-bridge"
#define DEFAULT_RING_CAPACITY  65536
#define DEFAULT_RECV_BUF       (4 * 1024 * 1024)
#define DEFAULT_STATS_INTERVAL 5
#define MAX_UDP_THREADS        32
#define MAX_DATAGRAM           65535

/* ─── Global state ───────────────────────────────────────────────── */
static volatile sig_atomic_t g_running   = 1;
static volatile sig_atomic_t g_connected = 0;
static stats_t               g_stats;

typedef struct {
    int                thread_id;
    int                udp_port;
    int                recv_buf_size;
    const char        *topic_prefix;
    ring_buffer_t     *ring;
} udp_thread_ctx_t;

typedef struct {
    MQTTAsync          client;
    ring_buffer_t     *rings;
    int                n_rings;
    int                stats_interval;
} mqtt_thread_ctx_t;

/* ─── Signal handler ─────────────────────────────────────────────── */
static void sig_handler(int sig) {
    (void)sig;
    g_running = 0;
}

/* ─── UDP Receiver Thread ────────────────────────────────────────── */
static void *udp_receiver_thread(void *arg)
{
    udp_thread_ctx_t *ctx = (udp_thread_ctx_t *)arg;

    /* Create UDP socket with SO_REUSEPORT */
    int fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (fd < 0) {
        perror("socket");
        return NULL;
    }

    int opt = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &opt, sizeof(opt));
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));
    setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &ctx->recv_buf_size,
               sizeof(ctx->recv_buf_size));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family      = AF_INET;
    addr.sin_addr.s_addr = INADDR_ANY;
    addr.sin_port        = htons(ctx->udp_port);

    /* Set recv timeout so threads can check g_running and exit */
    struct timeval tv = { .tv_sec = 1, .tv_usec = 0 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind");
        close(fd);
        return NULL;
    }

    printf("[UDP-%d] Listening on port %d\n", ctx->thread_id, ctx->udp_port);

    uint8_t buf[MAX_DATAGRAM];
    sensor_packet_t pkt;
    char topic[256];
    char json[4096];

    while (g_running) {
        ssize_t n = recv(fd, buf, sizeof(buf), 0);
        if (n <= 0) {
            if (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)
                continue;
            break;
        }

        stats_record_udp_recv(&g_stats, (uint64_t)n);

        /* Parse binary sensor packet */
        if (sensor_parse_binary(buf, (size_t)n, &pkt) != 0) {
            stats_record_parse_error(&g_stats);
            continue;
        }

        /* Build topic string */
        int tlen = snprintf(topic, sizeof(topic), "%s/%u",
                            ctx->topic_prefix, pkt.sensor_id);
        if (tlen < 0 || tlen >= (int)sizeof(topic)) {
            stats_record_parse_error(&g_stats);
            continue;
        }

        /* Serialize to JSON for MQTT payload */
        int jlen = sensor_to_json(&pkt, json, sizeof(json));
        if (jlen < 0) {
            stats_record_parse_error(&g_stats);
            continue;
        }

        /* Push to lock-free ring buffer */
        if (ring_try_push(ctx->ring, topic, (uint16_t)tlen,
                          json, (uint16_t)jlen) != 0) {
            stats_record_channel_drop(&g_stats);
        }
    }

    close(fd);
    printf("[UDP-%d] Stopped\n", ctx->thread_id);
    return NULL;
}

/* ─── MQTT connection callbacks ──────────────────────────────────── */
static void on_connect(void *context, MQTTAsync_successData *response) {
    (void)context; (void)response;
    g_connected = 1;
    printf("[MQTT] Connected to broker\n");
}

static void on_connect_failure(void *context, MQTTAsync_failureData *response) {
    (void)context;
    printf("[MQTT] Connection failed, rc=%d\n",
           response ? response->code : -1);
}

static void on_connection_lost(void *context, char *cause) {
    (void)context;
    g_connected = 0;
    printf("[MQTT] Connection lost: %s — reconnecting...\n",
           cause ? cause : "unknown");
}

/* ─── MQTT Publisher Thread ──────────────────────────────────────── */
static void *mqtt_publisher_thread(void *arg)
{
    mqtt_thread_ctx_t *ctx = (mqtt_thread_ctx_t *)arg;

    printf("[MQTT] Publisher thread started, draining %d ring(s)\n",
           ctx->n_rings);

    ring_slot_t slot;
    struct timespec ts_last, ts_now;
    clock_gettime(CLOCK_MONOTONIC, &ts_last);

    uint64_t spin_count = 0;

    while (g_running) {
        int got_any = 0;

        /* Round-robin drain all per-thread rings */
        for (int i = 0; i < ctx->n_rings; i++) {
            if (ring_try_pop(&ctx->rings[i], &slot) == 0) {
                got_any = 1;

                /* Extract topic and payload from slot */
                char topic[256];
                memcpy(topic, slot.data, slot.topic_len);
                topic[slot.topic_len] = '\0';

                /* Fire-and-forget QoS 0 publish */
                MQTTAsync_message msg = MQTTAsync_message_initializer;
                msg.payload    = slot.data + slot.topic_len;
                msg.payloadlen = slot.payload_len;
                msg.qos        = 0;
                msg.retained   = 0;

                MQTTAsync_responseOptions resp = MQTTAsync_responseOptions_initializer;
                int rc = MQTTAsync_sendMessage(ctx->client, topic, &msg, &resp);
                if (rc == MQTTASYNC_SUCCESS) {
                    stats_record_mqtt_publish(&g_stats);
                } else {
                    stats_record_mqtt_error(&g_stats);
                    /* Log first few errors to help debug */
                    static _Atomic int err_logged = 0;
                    if (atomic_fetch_add(&err_logged, 1) < 5)
                        printf("[MQTT] sendMessage rc=%d for topic=%s\n", rc, topic);
                }
            }
        }

        if (!got_any) {
            /* Busy-wait with backoff to keep latency low */
            spin_count++;
            if (spin_count > 1000) {
                /* Yield after sustained empty spins */
                struct timespec ts = {0, 100000}; /* 100µs */
                nanosleep(&ts, NULL);
                spin_count = 0;
            } else {
                __builtin_ia32_pause(); /* x86 PAUSE instruction */
            }
        } else {
            spin_count = 0;
        }

        /* Periodic stats */
        if (ctx->stats_interval > 0) {
            clock_gettime(CLOCK_MONOTONIC, &ts_now);
            double elapsed = (ts_now.tv_sec - ts_last.tv_sec) +
                             (ts_now.tv_nsec - ts_last.tv_nsec) / 1e9;
            if (elapsed >= ctx->stats_interval) {
                stats_print_and_reset(&g_stats, elapsed);
                ts_last = ts_now;
            }
        }
    }

    printf("[MQTT] Publisher thread stopped\n");
    return NULL;
}

/* ─── Main ───────────────────────────────────────────────────────── */
static void usage(const char *prog) {
    printf("Usage: %s [options]\n"
           "  --udp-port N       UDP listen port (default %d)\n"
           "  --mqtt-host H      MQTT broker host (default %s)\n"
           "  --mqtt-port N      MQTT broker port (default %d)\n"
           "  --topic-prefix P   MQTT topic prefix (default %s)\n"
           "  --threads N        UDP receiver threads (default: nproc)\n"
           "  --ring-cap N       Ring buffer capacity (default %d)\n"
           "  --stats-interval N Stats print interval secs (default %d, 0=off)\n"
           "  --help             Show this help\n",
           prog, DEFAULT_UDP_PORT, DEFAULT_MQTT_HOST, DEFAULT_MQTT_PORT,
           DEFAULT_TOPIC_PREFIX, DEFAULT_RING_CAPACITY, DEFAULT_STATS_INTERVAL);
}

int main(int argc, char *argv[])
{
    int         udp_port       = DEFAULT_UDP_PORT;
    const char *mqtt_host      = DEFAULT_MQTT_HOST;
    int         mqtt_port      = DEFAULT_MQTT_PORT;
    const char *topic_prefix   = DEFAULT_TOPIC_PREFIX;
    int         n_threads      = (int)sysconf(_SC_NPROCESSORS_ONLN);
    int         ring_cap       = DEFAULT_RING_CAPACITY;
    int         recv_buf       = DEFAULT_RECV_BUF;
    int         stats_interval = DEFAULT_STATS_INTERVAL;

    /* Minimal arg parsing */
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--udp-port") && i+1 < argc)
            udp_port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--mqtt-host") && i+1 < argc)
            mqtt_host = argv[++i];
        else if (!strcmp(argv[i], "--mqtt-port") && i+1 < argc)
            mqtt_port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--topic-prefix") && i+1 < argc)
            topic_prefix = argv[++i];
        else if (!strcmp(argv[i], "--threads") && i+1 < argc)
            n_threads = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--ring-cap") && i+1 < argc)
            ring_cap = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--stats-interval") && i+1 < argc)
            stats_interval = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--help")) {
            usage(argv[0]);
            return 0;
        }
    }

    if (n_threads < 1) n_threads = 1;
    if (n_threads > MAX_UDP_THREADS) n_threads = MAX_UDP_THREADS;

    printf("=== vad-sensor-bridge (C) ===\n");
    printf("UDP port:       %d\n", udp_port);
    printf("MQTT broker:    %s:%d\n", mqtt_host, mqtt_port);
    printf("Topic prefix:   %s\n", topic_prefix);
    printf("UDP threads:    %d\n", n_threads);
    printf("Ring capacity:  %d\n", ring_cap);
    printf("Stats interval: %ds\n", stats_interval);

    /* Setup signal handlers */
    signal(SIGINT,  sig_handler);
    signal(SIGTERM, sig_handler);

    /* Init stats */
    stats_init(&g_stats);

    /* Create per-thread ring buffers */
    ring_buffer_t *rings = (ring_buffer_t *)calloc(n_threads, sizeof(ring_buffer_t));
    for (int i = 0; i < n_threads; i++) {
        if (ring_init(&rings[i], ring_cap) != 0) {
            fprintf(stderr, "Failed to allocate ring buffer %d\n", i);
            return 1;
        }
    }

    /* ── MQTT client setup ── */
    char mqtt_uri[256];
    snprintf(mqtt_uri, sizeof(mqtt_uri), "tcp://%s:%d", mqtt_host, mqtt_port);

    MQTTAsync client;
    MQTTAsync_createOptions create_opts = MQTTAsync_createOptions_initializer;
    create_opts.sendWhileDisconnected = 1;
    create_opts.maxBufferedMessages   = 65536;

    int rc = MQTTAsync_createWithOptions(&client, mqtt_uri, DEFAULT_CLIENT_ID,
                                          MQTTCLIENT_PERSISTENCE_NONE, NULL,
                                          &create_opts);
    if (rc != MQTTASYNC_SUCCESS) {
        fprintf(stderr, "MQTT create failed: %d\n", rc);
        return 1;
    }

    MQTTAsync_setCallbacks(client, NULL, on_connection_lost, NULL, NULL);

    MQTTAsync_connectOptions conn_opts = MQTTAsync_connectOptions_initializer;
    conn_opts.keepAliveInterval = 30;
    conn_opts.cleansession      = 1;
    conn_opts.onSuccess         = on_connect;
    conn_opts.onFailure         = on_connect_failure;
    conn_opts.automaticReconnect = 1;
    conn_opts.minRetryInterval   = 1;
    conn_opts.maxRetryInterval   = 10;

    rc = MQTTAsync_connect(client, &conn_opts);
    if (rc != MQTTASYNC_SUCCESS) {
        fprintf(stderr, "MQTT connect failed: %d\n", rc);
        return 1;
    }

    /* Wait for MQTT connection (up to 5 seconds) */
    for (int w = 0; w < 50 && !g_connected && g_running; w++)
        usleep(100000);
    if (!g_connected) {
        fprintf(stderr, "⚠️  MQTT not connected yet — will retry in background\n");
    }

    /* ── Spawn UDP receiver threads ── */
    pthread_t udp_tids[MAX_UDP_THREADS];
    udp_thread_ctx_t udp_ctxs[MAX_UDP_THREADS];

    for (int i = 0; i < n_threads; i++) {
        udp_ctxs[i].thread_id    = i;
        udp_ctxs[i].udp_port     = udp_port;
        udp_ctxs[i].recv_buf_size = recv_buf;
        udp_ctxs[i].topic_prefix = topic_prefix;
        udp_ctxs[i].ring         = &rings[i];

        pthread_create(&udp_tids[i], NULL, udp_receiver_thread, &udp_ctxs[i]);

        /* Pin thread to CPU core for cache locality */
        cpu_set_t cpuset;
        CPU_ZERO(&cpuset);
        CPU_SET(i % sysconf(_SC_NPROCESSORS_ONLN), &cpuset);
        pthread_setaffinity_np(udp_tids[i], sizeof(cpuset), &cpuset);
    }

    /* ── Spawn MQTT publisher thread ── */
    pthread_t mqtt_tid;
    mqtt_thread_ctx_t mqtt_ctx;
    mqtt_ctx.client         = client;
    mqtt_ctx.rings          = rings;
    mqtt_ctx.n_rings        = n_threads;
    mqtt_ctx.stats_interval = stats_interval;

    pthread_create(&mqtt_tid, NULL, mqtt_publisher_thread, &mqtt_ctx);

    /* Pin MQTT thread to a dedicated core */
    {
        cpu_set_t cpuset;
        CPU_ZERO(&cpuset);
        CPU_SET(n_threads % (int)sysconf(_SC_NPROCESSORS_ONLN), &cpuset);
        pthread_setaffinity_np(mqtt_tid, sizeof(cpuset), &cpuset);
    }

    printf("✅ All systems go — listening for sensor data\n");

    /* ── Wait for shutdown ── */
    for (int i = 0; i < n_threads; i++)
        pthread_join(udp_tids[i], NULL);
    pthread_join(mqtt_tid, NULL);

    /* Cleanup */
    MQTTAsync_disconnectOptions disc_opts = MQTTAsync_disconnectOptions_initializer;
    disc_opts.timeout = 1000;
    MQTTAsync_disconnect(client, &disc_opts);
    MQTTAsync_destroy(&client);

    for (int i = 0; i < n_threads; i++)
        ring_destroy(&rings[i]);
    free(rings);

    printf("Shutdown complete.\n");
    return 0;
}
