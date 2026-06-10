# rigbat — local build, install, and systemd user service.
# Run `make` or `make help` for the list of targets.

UNIT      := rigbat.service
UNIT_SRC  := packaging/$(UNIT)
UNIT_DIR  := $(HOME)/.config/systemd/user
BIN       := $(HOME)/.cargo/bin/rigbat

.DEFAULT_GOAL := help

.PHONY: help build run install service enable disable restart status logs uninstall

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Build the release binary
	cargo build --release

run: ## Run the tray in the foreground (Ctrl-C to stop)
	cargo run --release -- tray

install: ## Build and install the binary to ~/.cargo/bin
	cargo install --path . --force

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

uninstall: ## Stop the service, remove the unit and the binary
	-systemctl --user disable --now $(UNIT)
	-rm -f $(UNIT_DIR)/$(UNIT)
	systemctl --user daemon-reload
	-cargo uninstall rigbat
