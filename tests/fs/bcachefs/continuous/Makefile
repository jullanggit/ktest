PREFIX ?= /usr
BINDIR ?= $(PREFIX)/bin
CARGO ?= cargo
TARGET_DIR ?= target

.PHONY: all install clean

all:
	$(CARGO) build --release --target-dir $(TARGET_DIR)

install: all
	install -Dm755 $(TARGET_DIR)/release/continuous-bcachefs-test $(DESTDIR)$(BINDIR)/continuous-bcachefs-test

clean:
	rm -rf $(TARGET_DIR)
