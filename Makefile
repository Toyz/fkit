# fkit — common tasks.
#
#   make up      start everything (generates .env on first run)
#   make logs    follow the hub's logs
#   make down    stop, keeping data
#   make test    run the Rust and frontend checks

.DEFAULT_GOAL := help
.PHONY: help setup up down restart logs ps test build web clean nuke image push

help: ## Show this help
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

setup: ## Generate .env with random secrets (safe to re-run)
	@scripts/setup-env.sh

up: setup ## Build and start the hub + Postgres
	docker compose up -d --build
	@echo
	@echo "  fkit hub  ->  http://localhost:$${HUB_PORT:-7500}"
	@echo "  The first account you register becomes the administrator."

down: ## Stop everything (data volumes are kept)
	docker compose down

restart: ## Rebuild and restart the hub only
	docker compose up -d --build hub

logs: ## Follow the hub's logs
	docker compose logs -f hub

ps: ## Show container status
	docker compose ps

test: ## Run Rust tests and typecheck the frontend
	cargo test --workspace
	cd web && npx tsc --noEmit

build: ## Build release binaries and the web UI
	cargo build --release --workspace
	cd web && npm run build

web: ## Run the frontend dev server against a local hub
	cd web && npm run dev

# ---- publishing -----------------------------------------------------------
# REGISTRY/IMAGE is where `make push` sends it; TAG is what it is called there.
# A tag is also always pushed as :latest, because a deploy that says "latest"
# should mean the last thing you deliberately published.
REGISTRY ?= ghcr.io
IMAGE    ?= toyz/fkit-hub
TAG      ?= latest
# Most servers are x86_64 while this machine may not be, so the platform is
# explicit rather than "whatever I happen to be running".
PLATFORMS ?= linux/amd64

image: ## Build the deployment image locally (PLATFORMS=linux/amd64)
	docker buildx build --platform $(PLATFORMS) \
	  -t $(REGISTRY)/$(IMAGE):$(TAG) --load .

push: ## Build and push to the registry (make push TAG=v1)
	@echo "Pushing $(REGISTRY)/$(IMAGE):$(TAG) for $(PLATFORMS)"
	docker buildx build --platform $(PLATFORMS) \
	  -t $(REGISTRY)/$(IMAGE):$(TAG) \
	  -t $(REGISTRY)/$(IMAGE):latest \
	  --push .

clean: ## Remove build artifacts
	cargo clean
	rm -rf web/dist web/node_modules

nuke: ## Stop and DELETE all data volumes (irreversible)
	@printf 'This deletes every repository and account. Type "yes" to continue: ' \
	  && read ans && [ "$$ans" = yes ] && docker compose down -v || echo "aborted"
