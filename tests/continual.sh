#!/bin/sh

set -ev

echo $0

rm -rf $0.dir
mkdir $0.dir
cd $0.dir

git clone ../../bigbro

cd bigbro

echo this is a bug here >> bigbro.h

../../../fac --continual > continual-output &

sleep 1
while test -e .git/fac-lock; do
    sleep 1
done
sleep 1

cat continual-output
ls -lh continual-output

grep 'Build failed' continual-output

if grep 'Build succeeded' continual-output; then
    echo should not succeed
    exit 1
fi

git checkout bigbro.h

sleep 12 # sleep a bit to give time to start building
while test -e .git/fac-lock; do
    sleep 1
done

cat continual-output
ls -lh continual-output

grep 'Build succeeded' continual-output

ps
kill $(jobs -p)

echo after killing background child
ps

echo we passed!
