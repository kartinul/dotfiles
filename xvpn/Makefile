# xvpn — xray VPN manager
#
# Everything here runs as you. There is no root daemon: the supervisor is a
# user LaunchAgent (macOS) or a user systemd unit (Linux).
#
# The one exception is `on` mode, which needs sing-box's tun interface and so
# needs root on macOS. The supervisor reaches it through cached sudo rather
# than running as root — `sudo -v` once, then it is silent.

CARGO ?= cargo
BIN   := $(CURDIR)/bin/xvpn

# Installed under $HOME so no target ever needs sudo.
PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin

# Must match state_home() in src/config.rs, including the XDG override, or the
# Makefile and the binary end up watching different files.
XDG_STATE_HOME ?= $(HOME)/.local/state
STATE  := $(XDG_STATE_HOME)/xvpn
LOGS   := $(STATE)/xray.log $(STATE)/sing-box.log $(STATE)/supervisor.log

OS := $(shell uname -s)

AGENT_DIR := $(HOME)/Library/LaunchAgents
AGENT     := $(AGENT_DIR)/com.xvpn.plist
AGENT_ID  := gui/$(shell id -u)/com.xvpn

UNIT_DIR := $(HOME)/.config/systemd/user
UNIT     := $(UNIT_DIR)/xvpn.service

# Everything here is user-scoped by design. Under `sudo`, `id -u` is 0 and
# `gui/0` does not exist (launchctl: "Domain does not support specified
# action"), while `$(HOME)` silently becomes root's — so the agent would be
# written to the wrong place and never load. Refuse up front instead.
ifeq ($(shell id -u),0)
$(error run this as yourself, not with sudo — the agent belongs in your own GUI session)
endif

# Type-check the Linux build too: most of the platform split is invisible to a
# macOS-only build.
LINUX_TARGET ?= x86_64-unknown-linux-musl

.DEFAULT_GOAL := all
.PHONY: all help build install uninstall agent unagent restart stop supervise \
        on off selective status validate repair migrate \
        logs logs-xray logs-singbox logs-supervisor \
        check check-linux fmt clippy lint clean

all: install ## Build and install into ~/.local/bin (default)

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build: ## Compile the release binary into ./bin
	$(CARGO) build --release
	@mkdir -p $(CURDIR)/bin
	rm -f $(BIN)
	cp -f target/release/xvpn $(BIN)
	chmod +x $(BIN)
	@if [ "$(OS)" = "Darwin" ]; then codesign -s - --force $(BIN) 2>/dev/null || true; fi
	@echo "built $(BIN)"

install: ## Run install.sh to install dependencies, binary, and service agent
	@./install.sh

# --- supervisor as a user agent ----------------------------------------------
#
# A user agent runs in the user's session, so it needs no root and cannot write
# outside the user's own files. That is the whole reason state moved to XDG
# paths: the old /var/run and /var/log locations were root-owned, so every
# write failed silently and the status showed stale values.

agent: ## Install and load the supervisor as a user agent
ifeq ($(OS),Darwin)
	@mkdir -p $(AGENT_DIR) $(STATE)
	@sed -e 's|__XVPN_BIN__|$(BINDIR)/xvpn|g' \
	     -e 's|__XVPN_LOG__|$(STATE)/supervisor.log|g' \
	     com.xvpn.plist > $(AGENT)
	@launchctl bootout $(AGENT_ID) 2>/dev/null || true
	@# bootout is asynchronous: bootstrapping while the old job is still
	@# tearing down fails with EIO (5). Retry rather than report a failure
	@# that resolves itself a second later.
	@for i in 1 2 3 4 5; do \
		launchctl bootstrap gui/$$(id -u) $(AGENT) 2>/dev/null && break; \
		sleep 1; \
	done; \
	launchctl print gui/$$(id -u)/com.xvpn >/dev/null 2>&1 \
		|| { echo "could not load $(AGENT_ID)"; \
		     echo "try: launchctl bootstrap gui/$$(id -u) $(AGENT)"; exit 1; }
	@launchctl enable $(AGENT_ID) 2>/dev/null || true
	@echo "loaded $(AGENT_ID) — the supervisor starts on its own (RunAtLoad)"
else
	@mkdir -p $(UNIT_DIR) $(STATE)
	@sed -e 's|__XVPN_BIN__|$(BINDIR)/xvpn|g' xvpn.service.in > $(UNIT)
	@systemctl --user daemon-reload
	@systemctl --user enable --now xvpn.service
	@echo "enabled xvpn.service (user)"
endif

unagent: ## Unload and remove the user agent
ifeq ($(OS),Darwin)
	@launchctl bootout $(AGENT_ID) 2>/dev/null || true
	@rm -f $(AGENT)
	@echo "removed $(AGENT)"
else
	@systemctl --user disable --now xvpn.service 2>/dev/null || true
	@rm -f $(UNIT)
	@echo "removed $(UNIT)"
endif

uninstall: unagent ## Uninstall binary, logs, state and config (retaining xray/profiles)
	rm -f $(BINDIR)/xvpn
	rm -rf $(STATE)
	@if [ -d "$(HOME)/.config/xvpn" ]; then \
		rm -f $(HOME)/.config/xvpn/active; \
		rm -f $(HOME)/.config/xvpn/mode; \
		rm -f $(HOME)/.config/xvpn/order; \
		rm -f $(HOME)/.config/xvpn/xvpn.conf; \
		rm -rf $(HOME)/.config/xvpn/modes; \
		echo "uninstalled: configs under ~/.config/xvpn removed (retained profiles and xray.json)"; \
	fi

restart: ## Restart the supervisor in place, keeping it installed
ifeq ($(OS),Darwin)
	@launchctl kickstart -k $(AGENT_ID)
else
	@systemctl --user restart xvpn.service
endif

stop: ## Stop the supervisor and every proxy, leaving the agent installed
	@launchctl bootout $(AGENT_ID) 2>/dev/null || true
	@systemctl --user stop xvpn.service 2>/dev/null || true
	@pkill -x sing-box 2>/dev/null || true
	@pkill -x xray 2>/dev/null || true
	@# In `on` mode sing-box runs as root, so a plain pkill cannot reach it.
	@# -n never prompts; it just does nothing if no grant is cached.
	@sudo -n pkill -x sing-box 2>/dev/null || true
	@rm -f $(STATE)/current
	@echo "stopped"

supervise: build ## Run the supervisor in the foreground (Ctrl-C to stop)
	$(BIN) --supervise

# --- modes -------------------------------------------------------------------
#
# on/off just write the mode file; the supervisor reconciles and republishes
# what it actually did. `on` needs root on macOS for sing-box's tun interface —
# run `sudo -v` first, or the supervisor refuses with that reason instead of
# prompting where nobody can see it.

on: ## Route all traffic through the VPN
	$(BINDIR)/xvpn on

off: ## Route nothing
	$(BINDIR)/xvpn off

selective: ## Route only the apps and sites you listed
	$(BINDIR)/xvpn default

# --- inspection --------------------------------------------------------------

status: ## Show mode, profile, and what routes via xray
	@$(BINDIR)/xvpn

validate: ## Validate every config against the installed xray/sing-box
	$(BINDIR)/xvpn check

repair: ## Rewrite sing-box configs onto the current schema
	$(BINDIR)/xvpn repair

migrate: ## Move state out of a directory an older build used
	$(BINDIR)/xvpn migrate

logs: ## Watch supervisor + xray + sing-box logs
	@tail -f $(LOGS)

logs-xray: ## Watch the xray log
	@tail -f $(STATE)/xray.log

logs-singbox: ## Watch the sing-box log
	@tail -f $(STATE)/sing-box.log

logs-supervisor: ## Watch the supervisor log
	@tail -f $(STATE)/supervisor.log

# --- development -------------------------------------------------------------

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

clean: ## Remove build artifacts
	$(CARGO) clean
	rm -f $(BIN)