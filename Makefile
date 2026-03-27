# SVG Viewer — build & bundle
# Usage:
#   make bundle          Build the .app bundle at the current version
#   make bundle-patch    Bump patch (0.1.0 → 0.1.1), build, bundle, tag
#   make bundle-minor    Bump minor (0.1.0 → 0.2.0), build, bundle, tag
#   make bundle-major    Bump major (0.1.0 → 1.0.0), build, bundle, tag

SHELL := /bin/bash
APP_NAME := SVG Viewer
BUNDLE_DIR := target/release/bundle/osx
DIST_DIR := dist

# Read current version from Cargo.toml
VERSION := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

.PHONY: build bundle bundle-patch bundle-minor bundle-major clean version

version:
	@echo $(VERSION)

build:
	cargo build --release

bundle: build
	cargo bundle --release
	@echo ""
	@echo "✓ Bundled $(APP_NAME) v$(VERSION)"
	@ls -lh "$(BUNDLE_DIR)/$(APP_NAME).app/Contents/MacOS/"*
	@echo "  → $(BUNDLE_DIR)/$(APP_NAME).app"

bundle-patch:
	@$(MAKE) --no-print-directory _bump PART=patch
	@$(MAKE) --no-print-directory _release

bundle-minor:
	@$(MAKE) --no-print-directory _bump PART=minor
	@$(MAKE) --no-print-directory _release

bundle-major:
	@$(MAKE) --no-print-directory _bump PART=major
	@$(MAKE) --no-print-directory _release

# --- Internal targets ---

_bump:
	@OLD=$(VERSION); \
	IFS='.' read -r MAJOR MINOR PATCH <<< "$$OLD"; \
	case "$(PART)" in \
		major) MAJOR=$$((MAJOR + 1)); MINOR=0; PATCH=0 ;; \
		minor) MINOR=$$((MINOR + 1)); PATCH=0 ;; \
		patch) PATCH=$$((PATCH + 1)) ;; \
	esac; \
	NEW="$$MAJOR.$$MINOR.$$PATCH"; \
	sed -i '' "s/^version = \"$$OLD\"/version = \"$$NEW\"/" Cargo.toml; \
	echo "  $(PART): $$OLD → $$NEW"

_release:
	$(eval NEW_VERSION := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/'))
	@$(MAKE) --no-print-directory bundle VERSION=$(NEW_VERSION)
	@mkdir -p $(DIST_DIR)
	@rm -rf "$(DIST_DIR)/$(APP_NAME).app"
	@cp -R "$(BUNDLE_DIR)/$(APP_NAME).app" "$(DIST_DIR)/"
	@echo ""
	@echo "✓ Copied to $(DIST_DIR)/$(APP_NAME).app"
	@# Commit version bump and tag
	@git add Cargo.toml Cargo.lock
	@git commit -m "v$(NEW_VERSION)"
	@git tag "v$(NEW_VERSION)"
	@echo "✓ Tagged v$(NEW_VERSION)"

clean:
	cargo clean
	rm -rf $(DIST_DIR)
