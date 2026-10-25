# kurin developer entry points.
#
# tier-0 e2e: three privileged systemd containers with sshd, driven by the
# kurin binary through preflight + prepare.
#
#   make tier0        # bring the cluster up (writes e2e/tier0/config.toml)
#   make tier0-test   # build kurin, assert plan + (if available) real apply
#   make tier0-down   # tear everything down
SHELL := /bin/bash
.DEFAULT_GOAL := help

KURIN_TARGET_DIR ?= target/tier0
KURIN_BIN := $(KURIN_TARGET_DIR)/debug/kurin
TIER0_DIR := e2e/tier0

.PHONY: help tier0 tier0-test tier0-down kurin

help:
	@echo "targets:"
	@echo "  tier0       bring up the 3-node systemd/sshd container cluster"
	@echo "  tier0-test  build kurin and run tier-0 assertions"
	@echo "  tier0-down  remove containers, network and generated config"

kurin: $(KURIN_BIN)

$(KURIN_BIN): $(shell find kurin kurin-config -path '*/target' -prune -o -type f \( -name '*.rs' -o -name '*.j2' -o -name '*.toml' \) -print 2>/dev/null) Cargo.toml Cargo.lock
	CARGO_TARGET_DIR=$(KURIN_TARGET_DIR) cargo build -p kurin

tier0:
	./$(TIER0_DIR)/up.sh

tier0-test: $(KURIN_BIN)
	python3 $(TIER0_DIR)/assert.py --bin $(KURIN_BIN) --config $(TIER0_DIR)/config.toml

tier0-down:
	./$(TIER0_DIR)/down.sh
.PHONY: tier1 tier1-test tier1-down
tier1:
	e2e/tier1/up.sh
tier1-test:
	CARGO_TARGET_DIR=target/tier1 cargo build -p kurin
	e2e/tier1/assert.sh target/tier1/debug/kurin e2e/tier1/config.toml
tier1-down:
	e2e/tier1/down.sh

.PHONY: tier2 tier2-test tier2-down
tier2:
	e2e/tier2/up.sh
tier2-test:
	CARGO_TARGET_DIR=target/tier2 cargo build -p kurin
	e2e/tier2/assert.sh target/tier2/debug/kurin e2e/tier2/config.toml
tier2-down:
	e2e/tier2/down.sh

.PHONY: tier3 tier3-test tier3-down
tier3:
	e2e/tier3/up.sh
tier3-test:
	CARGO_TARGET_DIR=target/tier3 cargo build -p kurin
	e2e/tier3/assert.sh target/tier3/debug/kurin e2e/tier3/config.toml
tier3-down:
	e2e/tier3/down.sh
