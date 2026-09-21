// Hex literals and bitwise operators, which this engine rejects at lex time —
// taking the whole file with them.
size = 0x10;              // 16
mask = (0xFF & 0x0F) + 1; // 16
shift = 1 << 2;           // 4
cube([size, mask, shift]);
