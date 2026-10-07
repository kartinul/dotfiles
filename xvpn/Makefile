# xvpn — xray VPN manager
#
# Targets work on both macOS and Linux; the service manager differs.

CARGO ?= cargo
BIN   := $(CURDIR)/bin/xvpn
PREFIX ?= /usr/local

OS    := $(shell uname -s)

# macOS: launchd agent. Linux: systemd unit (written by install.sh).
PLIST := /Library/LaunchDaemons/com.xvpn.plist
LABEL := system/com.xvpn

STATE := /var/run/xvpn.current

# Written by the supervisor.
XRAY_LOG    := /var/log/xvpn-xray.log
SINGBOX_LOG := /var/log/xvpn-singbox.log
SUP_LOG     := /var/log/xvpn-supervisor.log
LOGS        := $(XRAY_LOG) $(SINGBOX_LOG) $(SUP_LOG)

# Type-check the Linux build too: most of the platform split is invisible to a
# macOS-only build.
LINUX_TARGET ?= x86_64-unknown-linux-musl

.DEFAULT_GOAL := help
.PHONY: help build dev run check check-linux fmt clippy lint verify repair \
        supervise stop start restart reset status logs logs-xray logs-singbox \
        logs-supervisor clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build: ## Compile the release binary into ./bin
	$(CARGO) build --release
	cp -f target/release/xvpn $(BIN)
	chmod +x $(BIN)
	@echo "built $(BIN)"

install: build ## Install the binary to $(PREFIX)/bin (copies, no symlink)
	sudo mkdir -p $(PREFIX)/bin
	sudo cp -f $(BIN) $(PREFIX)/bin/xvpn
	sudo chmod +x $(PREFIX)/bin/xvpn
	@echo "installed $(PREFIX)/bin/xvpn"

dev: ## Compile the debug binary
	$(CARGO) build

run: dev ## Run the debug build (ARGS="apps set Telegram")
	./target/debug/xvpn $(ARGS)

supervise: build ## Run the supervisor in the foreground (Ctrl-C to stop)
	$(BIN) --supervise

check: ## Type-check
	$(CARGO) check

check-linux: ## Type-check for Linux, to catch platform-gated code
	$(CARGO) check --target $(LINUX_TARGET)

fmt: ## Format sources
	$(CARGO) fmt

clippy: ## Lint
	$(CARGO) clippy

lint: ## fmt check + clippy (warnings denied) + Linux check
	$(CARGO) fmt -- --check
	$(CARGO) clippy -- -D warnings
	$(MAKE) --no-print-directory check-linux

verify: build ## Validate every config against the installed xray/sing-box
	$(BIN) check

repair: build ## Rewrite sing-box configs onto the current schema
	$(BIN) repair

status: build ## Show mode, profile, and what routes via xray
	@$(BIN)

stop: ## Stop the supervisor and all proxies (needs sudo)
ifeq ($(OS),Darwin)
	@sudo launchctl bootout $(LABEL) 2>/dev/null || true
else
	@sudo systemctl stop xvpn.service 2>/dev/null || true
endif
	@sudo pkill -x sing-box 2>/dev/null || true
	@sudo pkill -x xray 2>/dev/null || true
	@sudo rm -f $(STATE)
	@echo "stopped"

start: ## Load the supervisor service (needs sudo)
ifeq ($(OS),Darwin)
	@sudo cp com.xvpn.plist $(PLIST)
	@sudo chown root:wheel $(PLIST)
	@sudo chmod 644 $(PLIST)
	@sudo launchctl bootout $(LABEL) 2>/dev/null || true
	@sudo rm -f /private/var/db/com.xvpn.launchd/com.xvpn.plist 2>/dev/null || true
	@sudo launchctl bootstrap system $(PLIST)
	@echo "started $(LABEL)"
else
	@sudo systemctl start xvpn.service 2>/dev/null \
		|| echo "run ./install.sh first to create the systemd unit"
	@echo "started xvpn.service"
endif

restart: ## Restart the supervisor service
	$(MAKE) --no-print-directory stop
	$(MAKE) --no-print-directory start

reset: build ## Stop everything, wipe generated configs, restore defaults, verify
	$(MAKE) --no-print-directory stop
	@echo
	@$(BIN) reset
	@echo
	$(MAKE) --no-print-directory verify

logs: ## Watch supervisor + xray + sing-box logs
	tail -f $(LOGS)

logs-xray: ## Watch the xray log
	tail -f $(XRAY_LOG)

logs-singbox: ## Watch the sing-box log
	tail -f $(SINGBOX_LOG)

logs-supervisor: ## Watch the supervisor log
	tail -f $(SUP_LOG)

clean: ## Remove build artifacts
	$(CARGO) clean
	rm -f $(BIN)