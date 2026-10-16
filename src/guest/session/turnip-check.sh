turnip=
if vulkaninfo --summary 2>/dev/null | grep -q 'driverName *= turnip'; then
    turnip=1
fi
