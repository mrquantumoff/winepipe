# WinePipe build. `make` builds everything into build/ in Wine's dll layout:
#
#   build/x86_64-unix/winepipe.so       Unix side (serves 64- and 32-bit)
#   build/x86_64-windows/winepipe.dll   64-bit PE driver
#   build/i386-windows/winepipe.dll     32-bit PE driver (WoW64)
#   build/bin/winepipe-test{64,32}.exe  test host
#   build/bin/winepipe-settings         settings app (libcosmic)
#
# `make driver` skips the settings app. Use the driver in place with
# WINEDLLPATH=$(pwd)/build, or `make install`.

CARGO     ?= cargo
CARGOFLAGS ?=
WINEBUILD ?= winebuild
PROFILE   ?= release
PREFIX    ?= /usr/local
# Wine's library directory (contains x86_64-unix/, x86_64-windows/, ...)
WINELIBDIR ?= $(shell for d in /usr/lib/wine /usr/lib64/wine /opt/wine-staging/lib/wine /opt/wine-stable/lib/wine /usr/local/lib/wine; do [ -d $$d/x86_64-unix ] && echo $$d && break; done)

T64 = x86_64-pc-windows-gnu
T32 = i686-pc-windows-gnu
OUT = build
APPID = io.github.mrquantumoff.WinePipe

.PHONY: all driver unix pe64 pe32 settings check test install install-driver install-settings uninstall clean

all: driver settings

driver: unix pe64 pe32

unix:
	$(CARGO) build $(CARGOFLAGS) --profile $(PROFILE) -p winepipe-unix
	install -Dm755 target/$(PROFILE)/libwinepipe.so $(OUT)/x86_64-unix/winepipe.so

pe64:
	$(CARGO) build $(CARGOFLAGS) --profile $(PROFILE) -p winepipe-pe -p winepipe-asiotest --target $(T64)
	install -Dm755 target/$(T64)/$(PROFILE)/winepipe.dll $(OUT)/x86_64-windows/winepipe.dll
	$(WINEBUILD) --builtin $(OUT)/x86_64-windows/winepipe.dll
	install -Dm755 target/$(T64)/$(PROFILE)/winepipe-test.exe $(OUT)/bin/winepipe-test64.exe

pe32:
	$(CARGO) build $(CARGOFLAGS) --profile $(PROFILE) -p winepipe-pe -p winepipe-asiotest --target $(T32)
	install -Dm755 target/$(T32)/$(PROFILE)/winepipe.dll $(OUT)/i386-windows/winepipe.dll
	$(WINEBUILD) --builtin $(OUT)/i386-windows/winepipe.dll
	install -Dm755 target/$(T32)/$(PROFILE)/winepipe-test.exe $(OUT)/bin/winepipe-test32.exe

settings:
	$(CARGO) build $(CARGOFLAGS) --profile $(PROFILE) -p winepipe-settings
	install -Dm755 target/$(PROFILE)/winepipe-settings $(OUT)/bin/winepipe-settings

check:
	$(CARGO) test $(CARGOFLAGS) --profile $(PROFILE) -p winepipe-unix -p winepipe-settings

test: driver
	./scripts/e2e-test.sh

install: install-driver install-settings

install-driver:
	@test -n "$(WINELIBDIR)" || { echo "WINELIBDIR not found; pass WINELIBDIR=/path/to/lib/wine"; exit 1; }
	install -Dm755 $(OUT)/x86_64-unix/winepipe.so $(DESTDIR)$(WINELIBDIR)/x86_64-unix/winepipe.so
	install -Dm644 $(OUT)/x86_64-windows/winepipe.dll $(DESTDIR)$(WINELIBDIR)/x86_64-windows/winepipe.dll
	install -Dm644 $(OUT)/i386-windows/winepipe.dll $(DESTDIR)$(WINELIBDIR)/i386-windows/winepipe.dll
	install -Dm755 scripts/winepipe-register $(DESTDIR)$(PREFIX)/bin/winepipe-register
	install -Dm755 scripts/winepipe-config $(DESTDIR)$(PREFIX)/bin/winepipe-config

install-settings:
	install -Dm755 $(OUT)/bin/winepipe-settings $(DESTDIR)$(PREFIX)/bin/winepipe-settings
	install -Dm644 data/$(APPID).desktop $(DESTDIR)$(PREFIX)/share/applications/$(APPID).desktop

uninstall:
	rm -f $(DESTDIR)$(WINELIBDIR)/x86_64-unix/winepipe.so \
	      $(DESTDIR)$(WINELIBDIR)/x86_64-windows/winepipe.dll \
	      $(DESTDIR)$(WINELIBDIR)/i386-windows/winepipe.dll \
	      $(DESTDIR)$(PREFIX)/bin/winepipe-register \
	      $(DESTDIR)$(PREFIX)/bin/winepipe-config \
	      $(DESTDIR)$(PREFIX)/bin/winepipe-settings \
	      $(DESTDIR)$(PREFIX)/share/applications/$(APPID).desktop

clean:
	$(CARGO) clean
	rm -rf $(OUT)
