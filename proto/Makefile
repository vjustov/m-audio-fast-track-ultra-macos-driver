CFLAGS += -O2 -Wall -Wextra $(shell pkg-config --cflags libusb-1.0)
LDLIBS += $(shell pkg-config --libs libusb-1.0) -lm

ftu_stream: ftu_stream.c

clean:
	rm -f ftu_stream

.PHONY: clean
