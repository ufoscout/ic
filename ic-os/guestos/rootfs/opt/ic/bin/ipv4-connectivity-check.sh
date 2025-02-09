#!/bin/bash

set -o nounset
set -o pipefail

METRICS_DIR="/run/node_exporter/collector_textfile"

write_metric() {
    local name=$1
    local value=$2
    local help=$3
    local type=$4

    echo -e "# HELP ${name} ${help}\n# TYPE ${type}\n${name} ${value}" >"${METRICS_DIR}/${name}.prom"
}

endpoints=("1.1.1.1" "8.8.8.8" "9.9.9.9")

connectivity_status=0

for endpoint in "${endpoints[@]}"; do
    # Using curl instead of ping as it requires less permissions
    if curl --connect-timeout 10 "${endpoint}" &>/dev/null; then
        connectivity_status=1
        break
    fi
done

write_metric "ipv4_connectivity_status" "${connectivity_status}" "Status of IPv4 connectivity" "gauge"
