# vad-sensor-bridge (C edition)
# High-performance UDP → MQTT QoS 0 sensor data bridge

CC       ?= gcc
CFLAGS   := -O3 -march=native -Wall -Wextra -Wpedantic -std=c11 \
            -D_GNU_SOURCE -I./include
LDFLAGS  := -lpthread -lpaho-mqtt3a

# Paho MQTT C library — adjust if installed in a non-standard location
# CFLAGS  += -I/usr/local/include
# LDFLAGS += -L/usr/local/lib

SRC_DIR  := src
BUILD_DIR := build
BIN      := $(BUILD_DIR)/vad-sensor-bridge

SRCS     := $(wildcard $(SRC_DIR)/*.c)
OBJS     := $(patsubst $(SRC_DIR)/%.c, $(BUILD_DIR)/%.o, $(SRCS))

.PHONY: all clean install-deps

all: $(BIN)

$(BUILD_DIR):
	mkdir -p $(BUILD_DIR)

$(BUILD_DIR)/%.o: $(SRC_DIR)/%.c | $(BUILD_DIR)
	$(CC) $(CFLAGS) -c $< -o $@

$(BIN): $(OBJS)
	$(CC) $(CFLAGS) $^ -o $@ $(LDFLAGS)
	@echo "✅ Built: $@"

clean:
	rm -rf $(BUILD_DIR)

# Install Paho MQTT C library from source (if not already installed)
install-deps:
	@echo "Installing Eclipse Paho MQTT C library..."
	git clone --depth 1 https://github.com/eclipse/paho.mqtt.c.git /tmp/paho-mqtt-c
	cd /tmp/paho-mqtt-c && \
		mkdir -p build && cd build && \
		cmake -DPAHO_WITH_SSL=OFF \
		      -DPAHO_BUILD_SHARED=ON \
		      -DPAHO_BUILD_STATIC=OFF \
		      -DCMAKE_INSTALL_PREFIX=/usr/local \
		      .. && \
		make -j$$(nproc) && \
		sudo make install && \
		sudo ldconfig
	rm -rf /tmp/paho-mqtt-c
	@echo "✅ Paho MQTT C installed"

# Run with defaults
run: $(BIN)
	./$(BIN)

# Run with perf stats
perf: $(BIN)
	perf stat -d ./$(BIN)
