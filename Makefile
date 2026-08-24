ID := jltrench.textify
SOURCE_DIR := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
PLUGIN_DIR := $(HOME)/.config/omarchy/plugins/$(ID)
QMLLINT := $(shell command -v qmllint || echo /usr/lib/qt6/bin/qmllint)

.PHONY: build install validate lint test remove clean

build:
	cargo build --release --manifest-path "$(SOURCE_DIR)/native/Cargo.toml"

## Build the binary and sync it into the Omarchy plugin folder.
install: build
	@mkdir -p "$(PLUGIN_DIR)/bin"
	@if [ "$(SOURCE_DIR)" != "$(PLUGIN_DIR)" ]; then \
		cp manifest.json BarWidget.qml Panel.qml icon.svg preview.png README.md LICENSE "$(PLUGIN_DIR)/"; \
	fi
	cp "$(SOURCE_DIR)/native/target/release/textify" "$(PLUGIN_DIR)/bin/textify"
	@echo "Installed $(ID) -> $(PLUGIN_DIR)"

validate:
	omarchy plugin validate "$(PLUGIN_DIR)"

lint:
	$(QMLLINT) -I "$${OMARCHY_PATH}/shell" BarWidget.qml Panel.qml

test:
	cargo test --manifest-path "$(SOURCE_DIR)/native/Cargo.toml"

remove:
	@if omarchy plugin remove $(ID) --yes; then \
		:; \
	elif [ -d "$(PLUGIN_DIR)" ]; then \
		rm -rf -- "$(PLUGIN_DIR)"; \
	fi

clean:
	cargo clean --manifest-path "$(SOURCE_DIR)/native/Cargo.toml"
