ID := jltrench.textify
PLUGIN_DIR := $(HOME)/.config/omarchy/plugins/$(ID)
QMLLINT := $(shell command -v qmllint || echo /usr/lib/qt6/bin/qmllint)

.PHONY: build install validate lint test remove clean

build:
	cargo build --release --manifest-path native/Cargo.toml

## Build the binary and sync everything into the Omarchy plugin folder.
install: build
	@mkdir -p $(PLUGIN_DIR)/bin
	cp manifest.json BarWidget.qml Panel.qml icon.svg README.md LICENSE $(PLUGIN_DIR)/
	cp native/target/release/textify $(PLUGIN_DIR)/bin/
	@echo "Installed $(ID) -> $(PLUGIN_DIR)"

validate:
	omarchy plugin validate $(PLUGIN_DIR)

lint:
	$(QMLLINT) -I "$${OMARCHY_PATH}/shell" BarWidget.qml Panel.qml

test:
	cargo test --manifest-path native/Cargo.toml

remove:
	@if omarchy plugin remove $(ID) --yes; then \
		:; \
	elif [ -d "$(PLUGIN_DIR)" ]; then \
		rm -rf -- "$(PLUGIN_DIR)"; \
	fi

clean:
	cargo clean --manifest-path native/Cargo.toml
