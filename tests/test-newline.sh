#!/bin/bash

set -ex

# test-newline
#
# First writes some text to a new file without a trailing newline, then
# writes some more text, this time with a trailing newline. The expected
# behavior from linemux is to print the two sets of text as a single
# line.

logdir="$1"
logfile="$logdir/foo.log"

sleep 0.1

echo -en "foo" > $logfile

sleep 0.1

echo -en " bar\n" >> $logfile

sleep 0.1

exit 0
