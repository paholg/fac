#!/bin/sh

set -ev

rm -rf $0.dir
mkdir $0.dir
cd $0.dir

echo foo > foo
echo bar > bar

cat > top.bilge <<EOF
| echo foo > foobar && echo baz > baz
> baz
c foobar

| echo foo > /tmp/junk && echo baz > bar
> bar
C /tmp

| echo foo >  .cache-me && cat .cache-read > localcache
> localcache
C .cache

| echo bar > foobar && echo foo > foo
> foo
EOF

echo good > .cache-read

../../bilge

cat top.bilge.done

if grep "$0.dir/.cache-me" top.bilge.done; then
  echo this file should be ignored
  exit 1
fi
if grep "$0.dir/.cache-read" top.bilge.done; then
  echo this file should be ignored
  exit 1
fi
grep "$0.dir/foobar" top.bilge.done


cat > top.bilge <<EOF
| echo foo > foobar && echo baz > baz
> baz
c foobar

| echo foo > /tmp/junk && echo baz > bar
> bar
C /tmp

| echo foo >  .cache-me && cat .cache-read > localcache
> localcache
C .cache

| echo bar > foobar && echo foo > foo
> foo
c oobar
EOF

rm foo

../../bilge
cat top.bilge.done

if grep "$0.dir/foobar" top.bilge.done; then
  echo this file should be ignored
  exit 1
fi

exit 0