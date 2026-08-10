BIN      := bin
BINARY   := $(BIN)/puppetterm-agent
GO       ?= go

.PHONY: build cross test vet smoke clean fmt

build:
	$(GO) build -o $(BINARY) ./cmd/puppetterm-agent

cross:
	GOOS=linux GOARCH=amd64 $(GO) build -o $(BIN)/puppetterm-agent-linux-amd64 ./cmd/puppetterm-agent
	GOOS=linux GOARCH=arm64 $(GO) build -o $(BIN)/puppetterm-agent-linux-arm64 ./cmd/puppetterm-agent

test:
	$(GO) test ./...

vet:
	$(GO) vet ./...

fmt:
	gofmt -l -w .

smoke: build
	./scripts/protocol-smoke.sh $(BINARY)

clean:
	rm -rf $(BIN)
