################################################################################
# runit
################################################################################
RUNIT_VERSION = 2.1.2
RUNIT_SITE = https://smarden.org/runit
RUNIT_LICENSE = BSD-3-Clause
RUNIT_LICENSE_FILES = package/COPYING
# Upstream archive has an admin/ prefix as well as its version directory.
RUNIT_STRIP_COMPONENTS = 2
RUNIT_BINS = runit runit-init runsv runsvdir sv chpst svlogd

define RUNIT_BUILD_CMDS
	printf '%s\n' '$(TARGET_CC) $(TARGET_CFLAGS) -std=gnu89 -D_GNU_SOURCE' > $(@D)/src/conf-cc
	printf '%s\n' '$(TARGET_CC) $(TARGET_LDFLAGS)' > $(@D)/src/conf-ld
	# Linux x86_64 ABI facts: avoid executing cross-compiled feature probes.
	cp $(@D)/src/hasshsgr.h1 $(@D)/src/hasshsgr.h
	cp $(@D)/src/iopause.h2 $(@D)/src/iopause.h
	cp $(@D)/src/uint64.h2 $(@D)/src/uint64.h
	$(TARGET_MAKE_ENV) $(MAKE) -C $(@D)/src -o hasshsgr.h -o iopause.h -o uint64.h $(RUNIT_BINS)
endef

define RUNIT_INSTALL_TARGET_CMDS
	$(foreach bin,$(RUNIT_BINS), \
		$(INSTALL) -D -m 0755 $(@D)/src/$(bin) $(TARGET_DIR)/sbin/$(bin)$(sep))
	ln -sf runit $(TARGET_DIR)/sbin/init
endef

$(eval $(generic-package))
