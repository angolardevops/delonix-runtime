# Thin wrappers over cargo and the Delonix build, so the common tasks are one
# word. `make help` lists them.
.DEFAULT_GOAL := help
PORT ?= __PORT__
IMAGE ?= __NAME__:dev

.PHONY: help
help: ## List the targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

.PHONY: dev
dev: ## Run the service locally (reads .env if present)
	cargo run

.PHONY: check
check: ## Format check, lint and test — what CI runs
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets

.PHONY: fmt
fmt: ## Format the code
	cargo fmt --all

.PHONY: build
build: ## Build the release binary
	cargo build --release

.PHONY: image
image: ## Build the container image with Delonix
	delonix build -t $(IMAGE) .

.PHONY: up
up: ## Bring the whole stack up (app + redis + rabbitmq)
	docker compose up -d --build

.PHONY: down
down: ## Stop the stack
	docker compose down

.PHONY: scan
scan: ## Run a SonarQube scan (needs the quality profile up)
	sonar-scanner

.PHONY: smoke
smoke: ## Smoke-test a running instance
	./scripts/smoke.sh http://localhost:$(PORT)
