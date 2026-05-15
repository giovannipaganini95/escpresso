#!/bin/bash
# Test barcode rendering
# Tests: GS k (1D barcodes), GS H/h/w, GS ( k cn=48 (PDF417), GS ( k cn=50 (DataMatrix)

echo "Testing barcode rendering..."

(
  # Initialize printer
  printf "\x1B\x40"

  # Header
  printf "\x1B\x61\x01"
  printf "=== BARCODE TEST ===\n\n"

  # --- EAN-13 ---
  printf "\x1B\x61\x00"
  printf "EAN-13 barcode:\n"
  # GS h 100 (height=100), GS w 2 (width=2), GS H 2 (HRI below)
  printf "\x1D\x68\x64"
  printf "\x1D\x77\x02"
  printf "\x1D\x48\x02"
  # GS k 67 (EAN-13, format B) 12 bytes + check digit
  printf "\x1D\x6B\x43\x0D4006381333931"
  printf "\n\n"

  # --- Code128 ---
  printf "Code128 barcode:\n"
  # GS h 80, GS w 3, GS H 2
  printf "\x1D\x68\x50"
  printf "\x1D\x77\x03"
  printf "\x1D\x48\x02"
  # GS k 73 (Code128, format B)
  printf "\x1D\x6B\x49\x0BHELLO-12345"
  printf "\n\n"

  # --- Code39 ---
  printf "Code39 barcode:\n"
  printf "\x1D\x68\x50"
  printf "\x1D\x77\x02"
  printf "\x1D\x48\x02"
  # GS k 69 (Code39, format B)
  printf "\x1D\x6B\x45\x07ABC-123"
  printf "\n\n"

  # --- Center aligned barcode ---
  printf "\x1B\x61\x01"
  printf "Centered EAN-8:\n"
  printf "\x1D\x68\x64"
  printf "\x1D\x77\x03"
  printf "\x1D\x48\x02"
  # GS k 68 (EAN-8, format B)
  printf "\x1D\x6B\x44\x0812345670"
  printf "\n\n"

  # --- PDF417 (GS ( k cn=48) ---
  printf "\x1B\x61\x00"
  printf "PDF417:\n"
  # Set module size: GS ( k pL pH cn fn n
  printf "\x1D\x28\x6B\x03\x00\x30\x43\x03"
  # Store data: GS ( k pL pH cn fn m d1...dk
  printf "\x1D\x28\x6B\x11\x00\x30\x50\x30Hello PDF417!!"
  # Print: GS ( k pL pH cn fn
  printf "\x1D\x28\x6B\x03\x00\x30\x52\x30"
  printf "\n\n"

  # --- DataMatrix (GS ( k cn=50) ---
  printf "DataMatrix:\n"
  # Set module size
  printf "\x1D\x28\x6B\x03\x00\x32\x43\x04"
  # Store data
  printf "\x1D\x28\x6B\x12\x00\x32\x50\x30Hello DataMatrix!"
  # Print
  printf "\x1D\x28\x6B\x03\x00\x32\x52\x30"
  printf "\n\n"

  # Paper cut
  printf "\x1D\x56\x00"

) | nc -w 2 localhost 9100

echo "Done."
