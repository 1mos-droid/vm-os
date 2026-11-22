################################################################################
# desktop-shell
################################################################################
DESKTOP_SHELL_VERSION = 0.1.0
DESKTOP_SHELL_SITE = $(BR2_EXTERNAL_HYPEROS_PATH)/../desktop-shell
DESKTOP_SHELL_SITE_METHOD = local
DESKTOP_SHELL_LICENSE = MIT OR Apache-2.0
DESKTOP_SHELL_DEPENDENCIES = libdrm libinput libxkbcommon wayland seatd udev fontconfig freetype host-pkgconf
DESKTOP_SHELL_CARGO_HOME = $(if $(CARGO_HOME),$(CARGO_HOME),$(DL_DIR)/cargo-home)

define DESKTOP_SHELL_BUILD_CMDS
	cd $(@D) && $(TARGET_MAKE_ENV) $(TARGET_CONFIGURE_OPTS) \
		CARGO_HOME="$(DESKTOP_SHELL_CARGO_HOME)" \
		HOST_CC="$(HOSTCC)" HOST_CXX="$(HOSTCXX)" \
		CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$(TARGET_CC)" \
		CC_x86_64_unknown_linux_musl="$(TARGET_CC)" \
		PKG_CONFIG_ALLOW_CROSS=1 \
		RUSTFLAGS="-C target-feature=-crt-static -L native=$(STAGING_DIR)/usr/lib" \
		cargo build --release --locked --target x86_64-unknown-linux-musl \
			--target-dir $(@D)/br-target --no-default-features --features kms
endef

define DESKTOP_SHELL_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/br-target/x86_64-unknown-linux-musl/release/desktop-shell $(TARGET_DIR)/usr/bin/desktop-shell
endef

$(eval $(generic-package))
