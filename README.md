# sdr-scanner

Scan and simultaneously decode multiple narrowband FM streams from a wide chunk of sprectrum.

# Prior Art

https://github.com/madengr/ham2mon - python glue, using gnuradio. Not
maintained. This is what I was using before, but it's not performant enough to
run on single board computers. (my fork https://github.com/masenf/ham2mon/tree/py3). This project has no intention of recreating the cursed curses interface this app uses... It's 2026, of course we're going to expose the controls via a web interface!

