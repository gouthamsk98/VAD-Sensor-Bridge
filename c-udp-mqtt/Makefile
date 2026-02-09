# vad-sensor-bridge (C edition)
# Multi-transport sensor data processor with VAD computation

CC       ?= gcc
CFLAGS   := -O3 -march=native -Wall -Wextra -Wpedantic -std=c11 \
            -D_GNU_SOURCE -I./include
LDFLAGS  := -lpthread -lmosquitto -lm

SRC_DIR  := src
BUILD_DIR := build
BIN      := $(BUILD_DIR)/vad-sensor-bridge

SRCS     := $(wildcard $(SRC_DIR)/*.c)
OBJS     := $(patsubst $(SRC_DIR)/%.c, $(BUILD_DIR)/%.o, $(SRCS))

.PHONY: all clean

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

run: $(BIN)
	./$(BIN)
