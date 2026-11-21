CARGO ?= cargo
PYTHON ?= python3
IMAGE ?= eventforge:local
PLATFORM ?= linux/amd64
CPUS ?= 8
MEM ?= 24g
DEV ?= .work/dev
OUT ?= .work/out
ANSWER ?= .work/dev-answer
SEED ?= 1
EVENTS ?= 1000000
CLICKS ?= 250000
GENERATOR := .work/generator/target/release/generate
export IMAGE PLATFORM CPUS MEM

.PHONY: native build test fixtures dev run-dev check-dev benchmark

native:
	$(CARGO) build --locked --release

build:
	docker buildx build --platform "$(PLATFORM)" --load -t "$(IMAGE)" .

test: native
	$(CARGO) test --locked --release
	$(PYTHON) scripts/run_fixtures.py fixtures -- target/release/eventforge
	$(PYTHON) -m unittest discover -s tests -v

fixtures: build
	$(PYTHON) scripts/run_fixtures.py fixtures -- scripts/docker_run.sh

$(GENERATOR): tools/generator.zip
	mkdir -p .work
	unzip -oq tools/generator.zip -d .work
	$(CARGO) build --release --locked --manifest-path .work/generator/Cargo.toml

dev: $(GENERATOR)
	$(GENERATOR) --seed "$(SEED)" --events "$(EVENTS)" --clicks "$(CLICKS)" --output "$(DEV)"

run-dev: build
	scripts/docker_run.sh "$(DEV)" "$(OUT)"

check-dev:
	$(PYTHON) scripts/compare.py "$(OUT)" "$(ANSWER)"

benchmark: native
	$(PYTHON) scripts/benchmark.py "$(DEV)" "$(OUT)" --threads "$(CPUS)"
