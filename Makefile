# Makefile — delonix-runtime (motor público, Apache-2.0).
#
# Contrato de imagens do monorepo NgolaCloud (ver ../Makefile). O ARTEFACTO
# PRIMÁRIO do runtime são os BINÁRIOS de host (`make binaries` → releases ghcr);
# a imagem CLI (`make image`) é secundária (CI / builder do DKS). Como o runtime
# não corre como workload k8s, `kind-load` é um no-op deliberado.
#
#   make binaries   compila `delonix` + `delonix-cri` (release) — artefacto primário
#   make image      constrói a imagem CLI delonix/runtime:$(TAG) (Delonixfile)
#   make ghcr-push  push da tag versionada para o ghcr
#   make kind-load  no-op (runtime não é workload k8s)
#   make image-tag  imprime a tag calculada
# Tag por git describe; nunca `latest`.

TAG        ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
IMAGE_REPO ?= delonix/runtime
IMAGE      := $(IMAGE_REPO):$(TAG)
GHCR_IMAGE ?= ghcr.io/angolardevops/delonix-runtime

.PHONY: help binaries image ghcr-push kind-load image-tag

help: ## Mostra esta ajuda
	@awk 'BEGIN{FS":.*## "} /^[a-zA-Z0-9_.-]+:.*## /{printf "  \033[36m%-14s\033[0m %s\n",$$1,$$2}' $(MAKEFILE_LIST)
	@echo "  TAG=$(TAG)"

binaries: ## Compila os binários de host (artefacto PRIMÁRIO)
	cargo build --release -p delonix-runtime-bin -p delonix-cri
	@echo "✓ target/release/{delonix,delonix-cri} ($(TAG))"

image: ## Constrói a imagem CLI delonix/runtime:$(TAG) (Delonixfile)
	DOCKER_BUILDKIT=1 docker build -f Delonixfile \
	  --build-arg DELONIX_VERSION=$(TAG) \
	  -t $(IMAGE) .
	@echo "✓ imagem $(IMAGE) construída (label delonix/runtime.version=$(TAG))"

ghcr-push: ## docker tag + push da imagem CLI para o ghcr (tag versionada)
	docker tag $(IMAGE) $(GHCR_IMAGE):$(TAG)
	docker push $(GHCR_IMAGE):$(TAG)
	@echo "✓ $(GHCR_IMAGE):$(TAG) publicada no ghcr"

kind-load: ## no-op — o runtime é um motor de host, não um workload k8s
	@echo "ℹ delonix-runtime não corre em k8s — nada a carregar no kind (no-op)."

image-tag: ## Imprime a tag versionada calculada
	@echo $(TAG)
