#!/bin/bash

# LavaExchange Proxy Salt Mining Script
# Mines for CREATE2 salts that produce gas-efficient addresses

FACTORY="0x231a4C35CA5b3Bd6bfFeC5CF59C0244376D7f851"
CALLER="0x0000000000000000000000000000000000000000"
INIT_CODE_HASH="0x822506bc36092401d63712c28a6a844017b4db6d0bc2723ebd0b80cf74de0113"

echo "Mining salt for LavaExchange proxy..."
echo "Factory: $FACTORY"
echo "Caller: $CALLER"
echo "Init Code Hash: $INIT_CODE_HASH"
echo ""

# Check if GPU device argument provided
if [ "$1" != "" ]; then
    DEVICE=$1
    LEADING_ZEROS=${2:-4}
    TOTAL_ZEROS=${3:-6}
    
    echo "Using GPU device $DEVICE with minimum $LEADING_ZEROS leading zeros and $TOTAL_ZEROS total zeros"
    cargo run --release --bin create2crunch "$FACTORY" "$CALLER" "$INIT_CODE_HASH" "$DEVICE" "$LEADING_ZEROS" "$TOTAL_ZEROS"
else
    echo "Using CPU mining (basic)"
    echo "To use GPU: ./run.sh <device_id> [leading_zeros] [total_zeros]"
    echo "Example: ./run.sh 0 4 6"
    echo ""
    cargo run --release --bin create2crunch "$FACTORY" "$CALLER" "$INIT_CODE_HASH"
fi