FROM debian:trixie

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    mmdebstrap debian-archive-keyring \
    curl file \
    xorriso squashfs-tools mtools dosfstools syslinux-common isolinux \
    xz-utils bzip2 p7zip-full unzip zstd cpio kmod util-linux lvm2 \
    busybox-static \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY . .