# Builds and tests every part. Installing needs root: sudo packaging/install.sh

HWTEST := soak latency ca_test

.PHONY: all daemon plugin panel hwtest hwcheck test clean

all: daemon plugin panel hwtest

daemon:
	cd daemon && cargo build --release

plugin:
	cmake -S plugin -B plugin/build -DCMAKE_BUILD_TYPE=Release
	cmake --build plugin/build

panel:
	panel/build.sh

# Tools that run against the device through Core Audio (see hwtest/*.swift headers).
hwtest: $(HWTEST:%=hwtest/build/%)

hwtest/build/%: hwtest/%.swift
	@mkdir -p hwtest/build
	swiftc -O $< -o $@

# The hardware checks with a verdict: needs the device, the installed driver and a
# loopback cable from output 1 to input 1 (see hwtest/run.py).
hwcheck: hwtest
	python3 -I hwtest/run.py

# Unit tests only; nothing here needs the device.
test:
	cd daemon && cargo test --release
	cd panel && swift test
	cmake -S plugin -B plugin/build -DCMAKE_BUILD_TYPE=Release
	cmake --build plugin/build --target daemon_link_tests
	plugin/build/daemon_link_tests

clean:
	cd daemon && cargo clean
	rm -rf plugin/build panel/.build panel/build hwtest/build
