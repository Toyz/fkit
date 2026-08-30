# fkit — common tasks.
#
#   make up      start everything in Docker (generates .env on first run)
#   make dev     run the hub on this machine with live reload
#   make dev-release  the same, optimized: slower to rebuild, far faster to
#                     push to, which matters once a repository is large
#   make logs    follow the hub's logs
#   make down    stop, keeping data
#   make test    run the Rust and frontend checks

.DEFAULT_GOAL := help
.PHONY: help setup up dev dev-release dev-db dev-down down restart logs ps test build web clean nuke image push

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

# ---- local development ----------------------------------------------------
# `make up` runs the hub in Docker, which is what you want to check a release.
# `make dev` runs it as a process on this machine so a change to a .rs file is
# a rebuild and not an image build.
dev: setup ## Run the hub here with live reload (Postgres stays in Docker)
	@scripts/dev.sh

dev-release: setup ## Run the hub here with live reload, optimized
	@RELEASE=1 scripts/dev.sh

dev-db: setup ## Start only Postgres, published on 55432 for a hub run by hand
	docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d --wait postgres

dev-down: ## Stop the development Postgres
	docker compose -f docker-compose.yml -f docker-compose.dev.yml down

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

web: ## Run the frontend dev server with hot reload (pairs with 'make dev')
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
