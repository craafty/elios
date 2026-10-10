#!/bin/sh
# A small tour of the shell, run with: sh demo.sh
echo "Hello from a script running on eliOS, pid $$."
echo "Arguments: $0 $1 $2"
echo "Making a scratch directory..."
mkdir -p /tmp/demo && cd /tmp/demo
seq 1 5 > numbers.txt
wc -l numbers.txt
grep 3 numbers.txt && echo "found a 3"
ls -l
cd / && rm -r /tmp/demo
echo "Cleaned up. Done!"
