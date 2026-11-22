################################################################################
# crosvm (source and sandbox pinned together)
################################################################################
CROSVM_VERSION = c41a3d34c2a0074262ae7f4630a5b45f7cb586ad
CROSVM_SITE = https://github.com/google/crosvm/archive
CROSVM_SOURCE = $(CROSVM_VERSION).tar.gz
CROSVM_LICENSE = BSD-3-Clause
CROSVM_LICENSE_FILES = LICENSE third_party/minijail/LICENSE
CROSVM_MINIJAIL_VERSION = 7845d89927a82dbdcdee6276837cd0978f69ba19
CROSVM_EXTRA_DOWNLOADS = https://chromium.googlesource.com/chromiumos/platform/minijail/+archive/$(CROSVM_MINIJAIL_VERSION).tar.gz
CROSVM_DEPENDENCIES = libcap wayland host-wayland host-pkgconf
CROSVM_CARGO_HOME = $(if $(CARGO_HOME),$(CARGO_HOME),$(DL_DIR)/cargo-home)
CROSVM_TARGET_DIR = $(@D)/br-target

define CROSVM_EXTRACT_MINIJAIL
	$(TAR) -xf $(CROSVM_DL_DIR)/$(CROSVM_MINIJAIL_VERSION).tar.gz -C $(@D)/third_party/minijail
endef
CROSVM_POST_EXTRACT_HOOKS += CROSVM_EXTRACT_MINIJAIL

# crosvm's 2D GPU links libwayland; use the image's musl loader and libraries.
define CROSVM_BUILD_CMDS
	cd $(@D) && $(TARGET_MAKE_ENV) $(TARGET_CONFIGURE_OPTS) \
		CARGO_HOME="$(CROSVM_CARGO_HOME)" \
		HOST_CC="$(HOSTCC)" HOST_CXX="$(HOSTCXX)" \
		CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$(TARGET_CC)" \
		CC_x86_64_unknown_linux_musl="$(TARGET_CC)" \
		AR_x86_64_unknown_linux_musl="$(TARGET_AR)" \
		CROSS_COMPILE="$(TARGET_CROSS)" \
		BINDGEN_EXTRA_CLANG_ARGS="--sysroot=$(STAGING_DIR) -I$(STAGING_DIR)/usr/include" \
		PKG_CONFIG_ALLOW_CROSS=1 \
		RUSTFLAGS="-C target-feature=-crt-static -L native=$(STAGING_DIR)/usr/lib" \
		cargo build --release --locked --target x86_64-unknown-linux-musl \
			--target-dir "$(CROSVM_TARGET_DIR)" --no-default-features \
			--features balloon,gpu,net
endef

define CROSVM_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(CROSVM_TARGET_DIR)/x86_64-unknown-linux-musl/release/crosvm $(TARGET_DIR)/usr/bin/crosvm
	mkdir -p $(TARGET_DIR)/usr/share/policy/crosvm
	cp -a $(@D)/jail/seccomp/x86_64/. $(TARGET_DIR)/usr/share/policy/crosvm/
endef

$(eval $(generic-package))
