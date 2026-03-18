#!/usr/bin/env bash
set -euo pipefail

ASSET_DIR="${ASSET_DIR:-/tmp/hive_assets}"
RETH_IMAGE_TAR="${RETH_IMAGE_TAR:-/tmp/reth_image/reth_image.tar}"

IMAGES=(
    "${ASSET_DIR}/hiveproxy.tar"
    "${ASSET_DIR}/eels_engine.tar"
    "${ASSET_DIR}/eels_rlp.tar"
    "${RETH_IMAGE_TAR}"
)

for image_tar in "${IMAGES[@]}"; do
    if [[ ! -f "${image_tar}" ]]; then
        echo "error: missing image tar ${image_tar}" >&2
        exit 1
    fi
    echo "Loading image ${image_tar}..."
    docker load -i "${image_tar}" &
done

wait

docker image ls -a
