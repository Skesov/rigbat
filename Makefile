# rigbat — local build, install, and systemd user service.
# Run `make` or `make help` for the list of targets.

# XDG base directories: the ~/.config and ~/.local/share paths below are only
# the spec's DEFAULTS. Honour the environment, or a user who moved them gets a
# unit systemd --user never searches and a menu entry nothing ever shows.
XDG_CONFIG := $(or $(XDG_CONFIG_HOME),$(HOME)/.config)
XDG_DATA   := $(or $(XDG_DATA_HOME),$(HOME)/.local/share)
# `cargo install` writes to $CARGO_HOME/bin; the unit's ExecStart has to point
# at the same place, so this is substituted into the unit at install time
# rather than hardcoded in the shipped file.
CARGO_BIN  := $(or $(CARGO_HOME),$(HOME)/.cargo)/bin

UNIT       := rigbat.service
UNIT_SRC   := packaging/$(UNIT)
UNIT_DIR   := $(XDG_CONFIG)/systemd/user
BIN        := $(CARGO_BIN)/rigbat
UDEV_RULE  := 70-rigbat.rules
UDEV_SRC   := packaging/$(UDEV_RULE)
UDEV_DEST  := /etc/udev/rules.d/$(UDEV_RULE)
AUTOSTART  := $(XDG_CONFIG)/autostart/rigbat.desktop
DESKTOP_SRC := packaging/rigbat.desktop
DESKTOP_DEST := $(XDG_DATA)/applications/rigbat.desktop
ICON_SRC   := packaging/icons/hicolor/scalable/apps/rigbat.svg
ICON_DEST  := $(XDG_DATA)/icons/hicolor/scalable/apps/rigbat.svg

.DEFAULT_GOAL := help

.PHONY: help build run test test-live install udev-install service enable disable restart status logs uninstall fmt lint gates

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Build the release binary
	cargo build --release

run: ## Run the tray in the foreground (Ctrl-C to stop)
	cargo run --release -- tray

test: ## Run the unit tests (the gate suite's test step)
	cargo test

fmt: ## Format the sources (the gate suite's format step)
	cargo fmt

lint: ## Deny-warnings clippy over every target (the gate suite's lint step)
	cargo clippy --all-targets -- -D warnings

gates: fmt lint test ## Run every gate that must pass before a commit
	@echo "All gates passed."

test-live: ## Run the #[ignore]d tests that need a live session (D-Bus, a writable temp dir)
	cargo test -- --ignored

install: ## Build and install the binary + desktop entry + icon
	cargo install --path . --force --locked
	install -Dm644 $(DESKTOP_SRC) $(DESKTOP_DEST)
	# Absolute Exec path: a desktop launcher is started by the DE, which does
	# not source an interactive shell, so $(CARGO_BIN) need not be on its PATH.
	sed -i 's|^Exec=rigbat |Exec=$(BIN) |' $(DESKTOP_DEST)
	install -Dm644 $(ICON_SRC) $(ICON_DEST)
	-update-desktop-database $(XDG_DATA)/applications 2>/dev/null

udev-install: ## Install the udev rule for USB HID devices (needs root: sudo make udev-install)
	install -m644 $(UDEV_SRC) $(UDEV_DEST)
	udevadm control --reload-rules
	udevadm trigger
	@echo "Installed $(UDEV_DEST). A device already plugged in has been re-triggered;"
	@echo "if it still shows offline, replug it."

service: install ## Install the systemd user unit (implies install)
	@mkdir -p $(UNIT_DIR)
	install -m644 $(UNIT_SRC) $(UNIT_DIR)/$(UNIT)
	# Point ExecStart at the binary cargo actually installed (honours CARGO_HOME).
	sed -i 's|^ExecStart=.*rigbat tray$$|ExecStart=$(BIN) tray|' $(UNIT_DIR)/$(UNIT)
	systemctl --user daemon-reload
	@echo "Installed $(UNIT_DIR)/$(UNIT). Enable with: make enable"

enable: ## Enable and start the service now
	systemctl --user enable --now $(UNIT)
	@echo "Started. Follow logs with: make logs"

disable: ## Stop and disable the service
	systemctl --user disable --now $(UNIT)

restart: ## Restart the service (after reinstalling the binary)
	systemctl --user restart $(UNIT)

status: ## Show service status
	systemctl --user status $(UNIT) --no-pager

logs: ## Follow service logs
	journalctl --user -u $(UNIT) -f

uninstall: ## Stop the service and remove everything installed (asks for sudo only if the udev rule is present)
	-systemctl --user disable --now $(UNIT)
	-rm -f $(UNIT_DIR)/$(UNIT)
	# Every step is prefixed with `-`: uninstall must finish even on a session
	# with no user bus (plain SSH, no lingering), where daemon-reload fails.
	-systemctl --user daemon-reload
	-rm -f $(AUTOSTART)
	-rm -f $(DESKTOP_DEST)
	-rm -f $(ICON_DEST)
	-update-desktop-database $(XDG_DATA)/applications 2>/dev/null
	# Only this step needs root, and only when udev-install was actually run.
	@if [ -e $(UDEV_DEST) ]; then \
		echo "Removing $(UDEV_DEST) (needs root)"; \
		sudo rm -f $(UDEV_DEST) && sudo udevadm control --reload-rules; \
	fi
	-cargo uninstall rigbat
	@echo "Config left in place: ~/.config/rigbat/config.json. Remove it yourself if wanted."
