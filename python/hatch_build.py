"""Puts the sidecar program inside the package when it is built for release.

An ordinary build leaves the package as plain Python with no sidecar in it,
which is what working on the package needs. `scripts/build-wheel.sh` builds
the sidecar first and then sets two variables that this file reads:

    ORVU_BUNDLE_ORVUD     the `orvud` program to put inside
    ORVU_WHEEL_PLATFORM   the kind of machine it runs on, as a wheel tag,
                          such as manylinux_2_17_x86_64

With ORVU_WHEEL_PLATFORM and no ORVU_BUNDLE_ORVUD, the package has no sidecar
inside and is made out to those machines only: that is the package for
Windows, which a machine with a package of its own never takes in its place.
"""

import os

from hatchling.builders.hooks.plugin.interface import BuildHookInterface


class BundleSidecar(BuildHookInterface):
    def initialize(self, version, build_data):
        orvud = os.environ.get("ORVU_BUNDLE_ORVUD")
        platform = os.environ.get("ORVU_WHEEL_PLATFORM")
        if version == "editable" or not (orvud or platform):
            return
        if not orvud:
            # No sidecar inside, made out to the machines named. Only Windows
            # may have a package with no sidecar: on another machine pip would
            # take it in place of the package with the sidecar inside.
            if not all(tag.startswith("win") for tag in platform.split(".")):
                raise RuntimeError(
                    f"ORVU_WHEEL_PLATFORM is {platform} and ORVU_BUNDLE_ORVUD is not set: only a package "
                    f"for Windows is built with no sidecar inside"
                )
            build_data["pure_python"] = False
            build_data["tag"] = f"py3-none-{platform}"
            return
        if not platform:
            raise RuntimeError("ORVU_BUNDLE_ORVUD is set, so ORVU_WHEEL_PLATFORM has to be set too")
        if not os.access(orvud, os.X_OK):
            raise RuntimeError(f"{orvud} is not a program that can be run")
        build_data["force_include"][orvud] = "orvu/_bin/orvud"
        # The package now only works on one kind of machine, and its name
        # has to say which.
        build_data["pure_python"] = False
        build_data["tag"] = f"py3-none-{platform}"
