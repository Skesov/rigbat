# rigbat — local build, install, and systemd user service.
# Run `make` or `make help` for the list of targets.

UNIT       := rigbat.service
UNIT_SRC   := packaging/$(UNIT)
UNIT_DIR   := $(HOME)/.config/systemd/user
BIN        := $(HOME)/.cargo/bin/rigbat
UDEV_RULE  := 70-rigbat.rules
UDEV_SRC   := packaging/$(UDEV_RULE)
UDEV_DEST  := /etc/udev/rules.d/$(UDEV_RULE)
AUTOSTART  := $(HOME)/.config/autostart/rigbat.desktop
DESKTOP_SRC := packaging/rigbat.desktop
DESKTOP_DEST := $(HOME)/.local/share/applications/rigbat.desktop
ICON_SRC   := packaging/icons/hicolor/scalable/apps/rigbat.svg
ICON_DEST  := $(HOME)/.local/share/icons/hicolor/scalable/apps/rigbat.svg

.DEFAULT_GOAL := help

.PHONY: help build run install udev-install service enable disable restart status logs uninstall

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Build the release binary
	cargo build --release

run: ## Run the tray in the foreground (Ctrl-C to stop)
	cargo run --release -- tray

install: ## Build and install the binary + desktop entry + icon
	cargo install --path . --force --locked
	install -Dm644 $(DESKTOP_SRC) $(DESKTOP_DEST)
	install -Dm644 $(ICON_SRC) $(ICON_DEST)
	-update-desktop-database $(HOME)/.local/share/applications 2>/dev/null
	-gtk-update-icon-cache $(HOME)/.local/share/icons/hicolor 2>/dev/null

udev-install: ## Install the udev rule for USB HID devices (needs root: sudo make udev-install)
	install -m644 $(UDEV_SRC) $(UDEV_DEST)
	udevadm control --reload-rules
	udevadm trigger
	@echo "Installed $(UDEV_DEST). A device already plugged in has been re-triggered;"
	@echo "if it still shows offline, replug it."

service: install ## Install the systemd user unit (implies install)
	@mkdir -p $(UNIT_DIR)
	install -m644 $(UNIT_SRC) $(UNIT_DIR)/$(UNIT)
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

uninstall: ## Stop the service, remove the unit, udev rule, desktop entry, icon, autostart entry and the binary
	-systemctl --user disable --now $(UNIT)
	-rm -f $(UNIT_DIR)/$(UNIT)
	systemctl --user daemon-reload
	-rm -f $(AUTOSTART)
	-rm -f $(DESKTOP_DEST)
	-rm -f $(ICON_DEST)
	-update-desktop-database $(HOME)/.local/share/applications 2>/dev/null
	-gtk-update-icon-cache $(HOME)/.local/share/icons/hicolor 2>/dev/null
	-sudo rm -f $(UDEV_DEST)
	-sudo udevadm control --reload-rules
	-cargo uninstall rigbat
	@echo "Config left in place: ~/.config/rigbat/config.json. Remove it yourself if wanted."
