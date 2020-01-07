#!/bin/bash

set -e

echo
echo "------------------------------------------------------------------------"
echo " Build Graph Node"
echo "========================================================================"
echo

cargo build -p graph-node

cd tests

echo
echo "------------------------------------------------------------------------"
echo " Overloaded contract functions"
echo "========================================================================"
echo

cd overloaded-contract-functions
yarn
graph test --standalone-node ../../target/debug/graph-node --node-logs "yarn test"
cd ..

cd ..
