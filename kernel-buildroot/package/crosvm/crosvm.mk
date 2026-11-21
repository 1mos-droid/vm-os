################################################################################
# crosvm
################################################################################
CROSVM_VERSION = pinned
CROSVM_LICENSE = BSD-3-Clause
CROSVM_PREBUILT_BIN = $(call qstrip,$(BR2_PACKAGE_CROSVM_PREBUILT_BIN))
CROSVM_SECCOMP_DIR = $(call qstrip,$(BR2_PACKAGE_CROSVM_SECCOMP_POLICY_DIR))

define CROSVM_INSTALL_TARGET_CMDS
	@if [ -z "$(CROSVM_PREBUILT_BIN)" ] || [ ! -x "$(CROSVM_PREBUILT_BIN)" ]; then \
		echo "FAIL: BR2_PACKAGE_CROSVM requires an executable BR2_PACKAGE_CROSVM_PREBUILT_BIN" >&2; \
		exit 1; \
	fi
	$(INSTALL) -D -m 0755 "$(CROSVM_PREBUILT_BIN)" "$(TARGET_DIR)/usr/bin/crosvm"
	@if [ -n "$(CROSVM_SECCOMP_DIR)" ]; then \
		if [ ! -d "$(CROSVM_SECCOMP_DIR)" ]; then \
			echo "FAIL: BR2_PACKAGE_CROSVM_SECCOMP_POLICY_DIR does not exist: $(CROSVM_SECCOMP_DIR)" >&2; \
			exit 1; \
		fi; \
		mkdir -p "$(TARGET_DIR)/usr/share/policy/crosvm"; \
		cp -a "$(CROSVM_SECCOMP_DIR)"/. "$(TARGET_DIR)/usr/share/policy/crosvm/"; \
	fi
endef

$(eval $(generic-package))
