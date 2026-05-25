BIN := ./target/release/mailsubsystem
DEV_ENV_DIR ?= ../mailsubsystem-dev-env

# ── Hobbyist workflow ────────────────────────────────────────────────────────

setup: ## Create core .env template and build release binary
	@if [ ! -f .env ]; then cp .env.example .env; echo "Created .env from .env.example"; fi
	cargo build --release
	@echo ""
	@echo "Core setup complete."
	@echo "For the local PostgreSQL + Dovecot sandbox, use:"
	@echo "  git clone https://github.com/iamcobolt/mailsubsystem-dev-env.git ../mailsubsystem-dev-env"
	@echo "  make dev-env-start"
	@echo "  make dev-env-core-env"

build: ## Build release binary
	cargo build --release

dev-env-start: ## Start companion PostgreSQL + Dovecot sandbox
	$(MAKE) -C $(DEV_ENV_DIR) start

dev-env-stop: ## Stop companion sandbox
	$(MAKE) -C $(DEV_ENV_DIR) stop

dev-env-reset: ## Reset companion sandbox data
	$(MAKE) -C $(DEV_ENV_DIR) reset

dev-env-core-env: ## Generate this repo's .env from companion sandbox config
	$(MAKE) -C $(DEV_ENV_DIR) core-env

dev-env-import: ## Import .eml/mbox into companion sandbox (set EMAILS=<path>)
	$(MAKE) -C $(DEV_ENV_DIR) import EMAILS="$(EMAILS)"

check: build ## Verify DB + IMAP connectivity
	$(BIN) check

test-llm-frontier: build ## Test frontier LLM provider
	$(BIN) test-llm --frontier

test-llm-local: build ## Test local LLM (LM Studio / Ollama)
	$(BIN) test-llm --local

# ── Pipeline commands ────────────────────────────────────────────────────────

sync: build ## Full envelope + body sync
	$(BIN) sync

analyze: build ## Batch AI analysis
	$(BIN) analyze

locate: build ## Agentic folder recommendations
	$(BIN) locate

file-dry-run: build ## Preview folder moves (no changes)
	$(BIN) file --dry-run

file: build ## Apply folder moves
	$(BIN) file

pipeline: build ## Run full pipeline: sync -> analyze -> locate -> file
	$(BIN) sync
	$(BIN) analyze
	$(BIN) embed-backfill
	$(BIN) locate
	$(BIN) file

# ── Operator workflow ────────────────────────────────────────────────────────

app: build ## Start server app: core coordinator + local API
	$(BIN) app

tui: build ## Start terminal UI
	$(BIN) tui

# ── Runtime / admin ──────────────────────────────────────────────────────────

core: build ## Run the normal local work coordinator (no HTTP API)
	$(BIN) core

core-apply: build ## Run core and allow approved IMAP filing moves
	CORE_FILE_APPLY=true $(BIN) core

core-status: build ## Show core queue depth, active work, pipeline timestamps, and recent errors
	$(BIN) core-status

core-smoke: build ## Verify the core DB/status path without starting HTTP API
	$(BIN) core-status

api: build ## Explicit API for wrappers (no core coordinator)
	$(BIN) api

migrate-schema: build ## Intentionally apply embedded schema.sql to configured database
	$(BIN) migrate-schema --apply

# ── Development ──────────────────────────────────────────────────────────────

test: ## Run tests
	cargo test

classification-eval: ## Run fixed classification regression eval corpus (set CORPUS=<path> to override)
	LOG_FILE_PATH=off cargo run -- classification-eval $(if $(CORPUS),--corpus $(CORPUS),)

lint: ## Run clippy + fmt check
	cargo clippy
	cargo fmt --check

# ── Help ─────────────────────────────────────────────────────────────────────

help: ## Show this help
	@printf "\033[36m%-20s\033[0m %s\n" "make app" "Start the server app (core + local API, warn+ console, info logs in logs/app.log)"
	@printf "\033[36m%-20s\033[0m %s\n" "make tui" "Start the terminal UI"
	@printf "\nAdvanced/admin commands are still available with: make advanced-help\n"

advanced-help: ## Show all advanced/admin targets
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

.DEFAULT_GOAL := help
.PHONY: setup build dev-env-start dev-env-stop dev-env-reset dev-env-core-env dev-env-import check test-llm-frontier test-llm-local \
        sync analyze locate file-dry-run file pipeline app core core-apply core-status core-smoke api tui migrate-schema \
        test classification-eval lint help advanced-help
