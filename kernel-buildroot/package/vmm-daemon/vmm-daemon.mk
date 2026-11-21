################################################################################
# vmm-daemon
################################################################################
VMM_DAEMON_VERSION = 0.1.0
VMM_DAEMON_SITE = $(BR2_EXTERNAL_HYPEROS_PATH)/../vmm-daemon
VMM_DAEMON_SITE_METHOD = local
VMM_DAEMON_LICENSE = MIT OR Apache-2.0

define VMM_DAEMON_BUILD_CMDS
	cd $(@D) && \
	CARGO_HOME="$(DL_DIR)/cargo-home" \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$(TARGET_CC)" \
	RUSTFLAGS="-C target-feature=+crt-static" \
	cargo build \
		--release \
		--locked \
		--target x86_64-unknown-linux-musl \
		--target-dir "$(@D)/br-target" \
		--manifest-path Cargo.toml
endef

define VMM_DAEMON_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 \
		$(@D)/br-target/x86_64-unknown-linux-musl/release/vmm-daemon \
		$(TARGET_DIR)/usr/bin/vmm-daemon
endef

$(eval $(generic-package))
