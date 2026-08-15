using System.Collections.Generic;
using Basis.BasisUI;
using UnityEngine;

namespace Basis.Media
{
    /// <summary>
    /// Builds the media player section of Settings &gt; Developer and registers it with
    /// the framework through <see cref="SettingsProvider.DeveloperSectionBuilders"/>, so
    /// the framework carries no reference to this package.
    /// </summary>
    public static class SettingsProviderMedia
    {
        [RuntimeInitializeOnLoadMethod]
        static void Register()
        {
            // Detach first: with domain reload disabled in the editor these statics survive
            // into the next play session, and a second Add would draw the section twice.
            SettingsProvider.DeveloperSectionBuilders.Remove(BuildSection);
            SettingsProvider.DeveloperSectionBuilders.Add(BuildSection);
            SettingsProvider.DeveloperResetActions.Remove(ResetDefaults);
            SettingsProvider.DeveloperResetActions.Add(ResetDefaults);
            BasisMediaSettings.EnsureLoaded();
        }

        public static void BuildSection(RectTransform container)
        {
            PanelElementDescriptor group =
                PanelElementDescriptor.CreateNew(PanelElementDescriptor.ElementStyles.Group, container);
            group.SetBackgroundVisible(false);
            group.SetTitle(BasisLocalization.Get("settings.media.title"));
            group.SetDescription(BasisLocalization.Get("settings.media.description"));

            PanelDropdown decode = PanelDropdown.CreateNewEntry(group.ContentParent);
            decode.Descriptor.SetTitle(BasisLocalization.Get("settings.media.decode"));
            decode.Descriptor.SetTooltip(BasisLocalization.Get("settings.media.decode.tooltip"));
            decode.AssignLocalizedEntries(
                new List<string>
                {
                    BasisMediaSettings.DecodeHardwareWithFallback,
                    BasisMediaSettings.DecodeHardwareOnly,
                    BasisMediaSettings.DecodeSoftwareOnly,
                },
                new List<string>
                {
                    "settings.media.decode.hardwareFallback",
                    "settings.media.decode.hardwareOnly",
                    "settings.media.decode.softwareOnly",
                });
            decode.AssignBinding(BasisMediaSettings.DecodePreference);

            PanelElementDescriptor engineStatus =
                PanelElementDescriptor.CreateNew(PanelElementDescriptor.ElementStyles.Group, group.ContentParent);
            engineStatus.SetTitle(BasisLocalization.Get("settings.media.engine.title"));
            engineStatus.SetDescription(DescribeEngine());
            engineStatus.IsolateAsCanvas();
        }

        /// <summary>Platform plus how many video routes the engine actually probed as
        /// hardware — the figure that says whether "hardware only" can play anything here.</summary>
        static string DescribeEngine()
        {
            BmCapabilitySet caps = BasisMediaCapabilities.Set;
            if (caps == null)
                return BasisLocalization.Get("settings.media.engine.unavailable");

            int hardware = 0, software = 0;
            if (caps.video != null)
            {
                foreach (BmVideoCap route in caps.video)
                {
                    if (route == null) continue;
                    if (route.route == "hardware") hardware++;
                    else software++;
                }
            }

            return BasisLocalization.Get("settings.media.engine.available",
                caps.platform, hardware, software);
        }

        static void ResetDefaults()
        {
            BasisMediaSettings.DecodePreference.ResetToDefault();
        }
    }
}
