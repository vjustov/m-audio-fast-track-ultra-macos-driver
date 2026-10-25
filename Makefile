# Builds and tests every part. Installing needs root: sudo packaging/install.sh

HWTEST := soak latency ca_test

.PHONY: all daemon plugin panel hwtest test clean

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

# Unit tests only; nothing here needs the device.
test:
	cd daemon && cargo test --release
	cd panel && swift test

clean:
	cd daemon && cargo clean
	rm -rf plugin/build panel/.build panel/build hwtest/build
