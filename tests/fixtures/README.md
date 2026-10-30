# Geolocation test fixtures

`GeoIP2-City-Test.mmdb` and `GeoIP2-Country-Test.mmdb` are unmodified synthetic
test databases from MaxMind-DB, commit
`d1551916cff0712d12ff602c309234471b0402a2`:
https://github.com/maxmind/MaxMind-DB/tree/d1551916cff0712d12ff602c309234471b0402a2/test-data

They are used under the MIT license in `LICENSE-MaxMind` and are not shipped as
production geolocation data. The country fixture checks that a country-only
database cannot silently disable city lookup.
