# SPDX-License-Identifier: Apache-2.0
import os
from a.b import c as d

class K:
    def m(self):
        return helper(self)

def helper(x):
    return os.getcwd()
