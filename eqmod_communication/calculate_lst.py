
from datetime import datetime
import math

# Inputs
year = 2026
month = 2
day = 17
hour = 17
minute = 40
second = 7

longitude_deg = 1.6092 # East is positive
latitude_deg = 42.9602 # North is positive

# Andromeda RA
andromeda_ra_hours = 0.75

# 1. Calculate Julian Date (JD)
def calculate_jd(year, month, day, hour, minute, second):
    if month <= 2:
        year -= 1
        month += 12
    
    A = math.floor(year / 100)
    B = 2 - A + math.floor(A / 4)
    
    jd = math.floor(365.25 * (year + 4716)) + math.floor(30.6001 * (month + 1)) + day + B - 1524.5
    
    # Add fractional day
    day_fraction = (hour + minute / 60.0 + second / 3600.0) / 24.0
    return jd + day_fraction

jd = calculate_jd(year, month, day, hour, minute, second)
print(f"Julian Date: {jd}")

# 2. Calculate GMST (Greenwich Mean Sidereal Time)
# Formula from "Astronomical Algorithms, 2nd Ed." by Jean Meeus (Approximate)
# GMST at 0h UT
# T = (JD0 - 2451545.0) / 36525
# GMST0 = 6h 41m 50.54841s + 8640184.812866s * T + 0.093104s * T^2 - 6.2e-6s * T^3
# Then add rotation for the current time.

# Simplified alternative often used:
# D = JD - 2451545.0
# GMST = 18.697374558 + 24.06570982441908 * D (hours)
# Normalize to 0-24

D = jd - 2451545.0
gmst_hours = 18.697374558 + 24.06570982441908 * D
gmst_hours = gmst_hours % 24
print(f"GMST: {gmst_hours} hours")

# 3. Calculate LST
# LST = GMST + Longitude (in hours)
# Longitude is East positive here. 15 degrees = 1 hour.
longitude_hours = longitude_deg / 15.0
lst_hours = gmst_hours + longitude_hours
lst_hours = lst_hours % 24

print(f"LST: {lst_hours} hours")

# Comparison
target_lst = 3.6238
diff = abs(lst_hours - target_lst)
print(f"Difference from target ({target_lst}): {diff} hours")

# 4. Andromeda Position relative to Meridian
# Hour Angle (HA) = LST - RA
# If HA is negative (or > 12h depending on normalization), it's East.
# If HA is positive (0 to 12h), it's West.
# Let's normalize HA to -12 to +12 range.

ha = lst_hours - andromeda_ra_hours
# Normalize to -12 to 12 range (or 0 to 24)
# Usually: 
# If LST < RA, object is East.
# If LST > RA, object is West.
# Taking into account the wrap around (24h)

# Correct normalization for determining direction
# HA needs to be between 0 and 24.
ha = ha % 24

direction = ""
if 0 < ha < 12:
    direction = "West"
else:
    direction = "East"

print(f"Andromeda RA: {andromeda_ra_hours} hours")
print(f"Hour Angle: {ha} hours")
print(f"Andromeda is {direction} of the Meridian")
