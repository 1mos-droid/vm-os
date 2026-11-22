################################################################################
# vm-control-ui
################################################################################
VM_CONTROL_UI_VERSION = 0.1.0
VM_CONTROL_UI_SITE = $(BR2_EXTERNAL_HYPEROS_PATH)/../vm-control-ui
VM_CONTROL_UI_SITE_METHOD = local
VM_CONTROL_UI_LICENSE = MIT OR Apache-2.0
VM_CONTROL_UI_DEPENDENCIES = wayland libxkbcommon fontconfig freetype host-pkgconf
VM_CONTROL_UI_CARGO_HOME = $(if $(CARGO_HOME),$(CARGO_HOME),$(DL_DIR)/cargo-home)

define VM_CONTROL_UI_BUILD_CMDS
	cd $(@D) && $(TARGET_MAKE_ENV) $(TARGET_CONFIGURE_OPTS) \
		CARGO_HOME="$(VM_CONTROL_UI_CARGO_HOME)" \
		HOST_CC="$(HOSTCC)" HOST_CXX="$(HOSTCXX)" \
		CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$(TARGET_CC)" \
		CC_x86_64_unknown_linux_musl="$(TARGET_CC)" \
		PKG_CONFIG_ALLOW_CROSS=1 \
		RUSTFLAGS="-C target-feature=-crt-static -L native=$(STAGING_DIR)/usr/lib" \
		cargo build --release --locked --target x86_64-unknown-linux-musl \
			--target-dir $(@D)/br-target --no-default-features --features wayland
endef

define VM_CONTROL_UI_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/br-target/x86_64-unknown-linux-musl/release/vm-control-ui $(TARGET_DIR)/usr/bin/vm-control-ui
endef

# Shared Strata design system (ui-kit/: Slint components and Inter fonts).
# build.rs finds it at ./ui-kit inside the build directory.
define VM_CONTROL_UI_COPY_UI_KIT
	rm -rf $(@D)/ui-kit
	cp -a $(BR2_EXTERNAL_HYPEROS_PATH)/../ui-kit $(@D)/ui-kit
endef
VM_CONTROL_UI_POST_RSYNC_HOOKS += VM_CONTROL_UI_COPY_UI_KIT

$(eval $(generic-package))
